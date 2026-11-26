#!/usr/bin/env python3
"""Run the web executable against PostgreSQL and a controllable finance HTTP endpoint."""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError, URLError

from http_fixture import local_http, postgres, web_env


class FinanceProbe(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/healthz/ready":
            self.server.received.set()
            self.server.release.wait(3)
            status = self.server.probe_status
            result = {"status": "ready" if status == 200 else "not_ready", "checks": []}
        elif self.path == "/api/v1/market/status":
            status = 200
            result = {"state": "ready", "source": "replay", "updated_at": "2024-01-01T00:00:00Z",
                      "max_age_seconds": 180, "instruments": [{"instrument": "SPY.ARCA",
                      "last_bar_at": "2024-01-01T00:00:00Z", "age_seconds": 0, "stale": False}]}
        else:
            raise AssertionError(f"unexpected upstream path {self.path}")
        data = json.dumps(result).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *_args):
        pass


def reserve_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def get(port, path):
    try:
        response = local_http.open(f"http://127.0.0.1:{port}{path}", timeout=8)
    except HTTPError as error:
        response = error
    with response:
        return response.status, response.headers, response.read()


def wait_ready(proc, port):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        assert proc.poll() is None, proc.returncode
        try:
            status, _, body = get(port, "/healthz/ready")
            if status == 200:
                return json.loads(body)
        except (OSError, URLError):
            pass
        time.sleep(.1)
    raise AssertionError("web did not become ready with the test PostgreSQL database")


def run(root, finance, socket_dir):
    static = root / "dist"
    assets = static / "assets"
    assets.mkdir(parents=True)
    notices = static / "third-party" / "lightweight-charts"
    notices.mkdir(parents=True)
    license_files = {
        "LICENSE": b"Apache License\nVersion 2.0, January 2004\n",
        "NOTICE": b"Lightweight Charts\nCopyright TradingView, Inc.\n",
    }
    for name, content in license_files.items():
        (notices / name).write_bytes(content)
    (static / "api").mkdir()
    (static / "api" / "static-shadow.txt").write_bytes(b"static file under an API path")
    (root / "outside.txt").write_bytes(b"file outside the static root")
    index = b"<!doctype html><title>SPA fallback</title>"
    (static / "index.html").write_bytes(index)
    (assets / "app-4da7eb55.js").write_bytes(b"export default 42;")
    port = reserve_port()
    env = web_env(root, port, finance.server_port, static=static, socket_dir=socket_dir)
    binary = os.environ["WEB_BIN"]
    subprocess.run([binary, "migrate"], env=env, check=True)
    proc = subprocess.Popen([binary, "serve"], env=env)
    try:
        print("ready=200", wait_ready(proc, port), flush=True)
        first, second = (get(port, "/healthz/live") for _ in range(2))
        assert first[0] == second[0] == 200
        assert first[1]["X-Request-ID"] and first[1]["X-Request-ID"] != second[1]["X-Request-ID"]
        for path in ("/", "/index.html", "/dashboard/deep-link", "/dashboard", "/trading",
                     "/backtests", "/backtests/v1.2/details", "/static-notes"):
            code, headers, body = get(port, path)
            assert code == 200 and body == index, (path, code, body[:100])
            assert "no-cache" in headers["Cache-Control"], (path, headers)
        code, headers, body = get(port, "/assets/app-4da7eb55.js")
        assert code == 200 and b"export default" in body and "immutable" in headers["Cache-Control"]
        assert headers["Cache-Control"] == "public, max-age=31536000, immutable"
        assert get(port, "/assets/missing.js")[0] == 404
        for name, content in license_files.items():
            path = f"/third-party/lightweight-charts/{name}"
            code, headers, body = get(port, path)
            assert code == 200 and body == content, (path, code, body[:100])
            assert headers["Cache-Control"] == "no-cache", (path, headers)
        for path in ("/NOTICE", "/%4EOTICE", "/./%2fNOTICE//"):
            code, headers, body = get(port, path)
            assert code == 200 and body == license_files["NOTICE"], (path, code, body[:100])
            assert body != index, path
            assert headers["Cache-Control"] == "no-cache", (path, headers)
        for path in (
            "/static/third-party/lightweight-charts/LICENSE",
            "/static/third-party/lightweight-charts/NOTICE",
            "/static", "/static/unknown", "/%73tatic/unknown",
            "/assets/", "/assets/unknown", "/third-party/", "/third-party/lightweight-charts/missing",
            "/missing.js", "/theme.css", "/bundle.js.map", "/favicon.ico", "/images/logo.PNG",
            "/fonts/app.woff2", "/missing%2ejs", "/docs/LICENSE", "/docs/NOTICE/",
            "/docs/NOTICE", "/docs%2fNOTICE", "/NOTICE/NOTICE",
        ):
            code, headers, body = get(port, path)
            assert code == 404 and body != index, (path, code, body[:100])
            assert headers["Cache-Control"] == "no-cache", (path, headers)
        for path in (
            "/dashboard/../third-party/missing", "/foo/../static/unknown",
            "/dashboard/%2e%2e/third-party/missing", "/foo/%2e./static/unknown",
            "/foo/../dashboard", "/foo/%2E%2E/index.html", "/../outside.txt", "/%2e%2e/outside.txt",
            "/docs/../NOTICE", "/docs/%2e%2e/NOTICE",
        ):
            code, headers, body = get(port, path)
            assert code == 404 and body == b"", (path, code, body[:100])
            assert headers["Cache-Control"] == "no-cache", (path, headers)
        docs = static / "docs"
        docs.mkdir()
        docs_notice = b"Documentation notice\n"
        (docs / "NOTICE").write_bytes(docs_notice)
        code, _, body = get(port, "/docs/NOTICE")
        assert code == 200 and body == docs_notice, (code, body[:100])
        (notices / "NOTICE").unlink()
        code, _, body = get(port, "/NOTICE")
        assert code == 404 and body != index, (code, body[:100])
        code, headers, body = get(port, "/api/unknown-path")
        assert code == 404 and headers.get_content_type() == "application/json" and isinstance(json.loads(body), dict)
        for path in (
            "/api", "/healthz", "/healthz/unknown-path",
            "/%61pi", "/%68ealthz", "/%61pi%2funknown-path", "/./%68ealthz//live",
            "/%61pi/static-shadow.txt", "/api/%FF", "/%68ealthz/%FF", "/%61pi/%2e%2e/dashboard",
        ):
            code, headers, body = get(port, path)
            assert code == 404 and headers.get_content_type() == "application/json", (path, code, body[:100])
            assert json.loads(body)["error"]["code"] == "not_found", (path, body[:100])
        code, _, body = get(port, "/api/v1/system/status")
        assert code == 200
        good = json.loads(body)
        assert good["market"]["fresh"] and good["market"]["instrument_count"] == 1, good
        finance.probe_status = 503
        assert get(port, "/healthz/ready")[0] == 200
        code, _, body = get(port, "/api/v1/system/status")
        degraded = json.loads(body)
        assert code == 200 and good["finance"]["ok"] and not degraded["finance"]["ok"]
        print("system/status (healthy and degraded)", good, degraded, flush=True)
        finance.probe_status = 200
        finance.received.clear()
        finance.release.clear()
        result = []
        client = threading.Thread(target=lambda: result.append(get(port, "/api/v1/system/status")))
        client.start()
        try:
            assert finance.received.wait(5), "no in-flight upstream probe"
            proc.send_signal(signal.SIGTERM)
            time.sleep(.1)
            assert proc.poll() is None, "terminated before draining the in-flight HTTP request"
        finally:
            finance.release.set()
            client.join(timeout=10)
        assert len(result) == 1 and result[0][0] == 200
        assert proc.wait(10) == 0
        print("PASS request IDs, SPA fallback, static license files, cache, API/health/assets 404, "
              "SIGTERM drained in-flight 200", flush=True)
    finally:
        finance.release.set()
        if proc.poll() is None:
            proc.kill()
            proc.wait(5)


if __name__ == "__main__":
    with ThreadingHTTPServer(("127.0.0.1", 0), FinanceProbe) as mock:
        mock.probe_status = 200
        mock.received = threading.Event()
        mock.release = threading.Event()
        mock.release.set()
        threading.Thread(target=mock.serve_forever, daemon=True).start()
        try:
            with tempfile.TemporaryDirectory(dir="/tmp") as folder:
                root = Path(folder)
                with postgres(root) as socket_dir:
                    run(root, mock, socket_dir)
        finally:
            mock.shutdown()
