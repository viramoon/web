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
from urllib.request import urlopen


class FinanceProbe(BaseHTTPRequestHandler):
    def do_GET(self):
        assert self.path == "/healthz/ready", self.path
        self.server.received.set()
        self.server.release.wait(3)
        status = self.server.probe_status
        result = {"status": "ready" if status == 200 else "not_ready", "checks": []}
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
        response = urlopen(f"http://127.0.0.1:{port}{path}", timeout=8)
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


def run(root, finance):
    static = root / "dist"
    assets = static / "assets"
    assets.mkdir(parents=True)
    index = b"<!doctype html><title>SPA fallback</title>"
    (static / "index.html").write_bytes(index)
    (assets / "app-4da7eb55.js").write_bytes(b"export default 42;")
    port = reserve_port()
    env = {
        **os.environ,
        "WEB_BIND_HOST": "127.0.0.1",
        "WEB_PORT": str(port),
        "WEB_STATIC_ROOT": str(static),
        "FINANCE_BASE_URL": f"http://127.0.0.1:{finance.server_port}",
    }
    binary = os.environ["WEB_BIN"]
    subprocess.run([binary, "migrate"], env=env, check=True)
    proc = subprocess.Popen([binary, "serve"], env=env)
    try:
        print("ready=200", wait_ready(proc, port), flush=True)
        first, second = (get(port, "/healthz/live") for _ in range(2))
        assert first[0] == second[0] == 200
        assert first[1]["X-Request-ID"] and first[1]["X-Request-ID"] != second[1]["X-Request-ID"]
        for path in ("/", "/index.html", "/dashboard/deep-link"):
            code, headers, body = get(port, path)
            assert code == 200 and body == index, (path, code, body[:100])
            assert "no-cache" in headers["Cache-Control"], (path, headers)
        code, headers, body = get(port, "/assets/app-4da7eb55.js")
        assert code == 200 and b"export default" in body and "immutable" in headers["Cache-Control"]
        assert get(port, "/assets/missing.js")[0] == 404
        code, headers, body = get(port, "/api/unknown-path")
        assert code == 404 and headers.get_content_type() == "application/json" and isinstance(json.loads(body), dict)
        code, _, body = get(port, "/api/v1/system/status")
        assert code == 200
        good = json.loads(body)
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
        print("PASS request IDs, SPA fallback, cache, API/assets 404, SIGTERM drained in-flight 200", flush=True)
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
            with tempfile.TemporaryDirectory() as folder:
                run(Path(folder), mock)
        finally:
            mock.shutdown()
