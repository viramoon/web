#!/usr/bin/env python3
"""Validate write authorization, idempotency and public reads through the web executable."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import secrets
import subprocess
import tempfile
import threading
import time
from urllib.error import HTTPError, URLError
from urllib.parse import parse_qs, urlsplit
from urllib.request import Request

from core_http import reserve_port
from finance_resilience import SAMPLE, SUMMARY, READY, STATUS, MARKET
from http_fixture import local_http, web_env


JOBS = "/api/v1/backtests"
JOB_ID = 101
INPUT = {
    "strategy": "DemoStrategy", "instrument": "SPY.ARCA", "params": {},
    "start": "2026-11-02T14:30:00Z", "end": "2026-11-02T15:30:00Z",
}
JOB = {
    "id": JOB_ID, "request": INPUT, "status": "succeeded", "attempt": 1,
    "created_at": INPUT["start"], "summary": SUMMARY,
}
LATER_JOB = {**JOB, "id": JOB_ID + 1}


def request(port, path, method="GET", headers=None, data=None):
    url = f"http://127.0.0.1:{port}{path}"
    req = Request(url, method=method, headers=headers or {}, data=data)
    try:
        response = local_http.open(req, timeout=8)
    except HTTPError as err:
        response = err
    with response:
        return response.status, response.headers, json.load(response)


def post(port, token=None, key=None, *, address=None, payload=None):
    headers = {"Content-Type": "application/json"}
    if token is not None:
        headers["Authorization"] = "Bearer " + token
    if key is not None:
        headers["Idempotency-Key"] = key
    if address is not None:
        headers["X-Forwarded-For"] = address
    return request(port, JOBS, "POST", headers, json.dumps(INPUT).encode() if payload is None else payload)


class Finance(BaseHTTPRequestHandler):
    def respond(self, code, data):
        raw = json.dumps(data).encode()
        try:
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)
        except (BrokenPipeError, ConnectionResetError):
            # The test deliberately waits out web's deadline for the first POST with one key.
            pass

    def note_auth(self):
        if "Authorization" in self.headers:
            self.server.auth_forwarded.set()

    def do_POST(self):
        self.note_auth()
        key = self.headers.get("Idempotency-Key")
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        with self.server.lock:
            self.server.submissions.append((self.path, key, body))
            first = sum(entry[1] == key for entry in self.server.submissions) == 1
        if self.path != JOBS or key is None:
            self.respond(422, {"error": {"code": "invalid", "message": "Missing key or invalid path"}})
        else:
            # The operation can commit at finance while web times out reading its response.
            if key == self.server.slow_key and first:
                self.server.resume.wait(8)
            self.respond(202, {"id": JOB_ID})

    def do_GET(self):
        self.note_auth()
        url = urlsplit(self.path)
        if url.path == SAMPLE:
            self.respond(200, SUMMARY)
        elif url.path == READY:
            self.respond(200, {"status": "ready", "checks": []})
        elif url.path == MARKET:
            self.respond(200, self.server.market)
        elif url.path == f"{JOBS}/datasets":
            self.respond(200, {"items": []})
        elif url.path == f"{JOBS}/by-key":
            key = self.headers.get("Idempotency-Key")
            with self.server.lock:
                found = any(entry[1] == key for entry in self.server.submissions)
            if found:
                self.respond(200, {"id": JOB_ID})
            else:
                self.respond(404, {"error": {"code": "job_not_found", "message": "No job for this key"}})
        elif url.path == JOBS:
            query = parse_qs(url.query)
            self.server.pagination = query
            limit = int(query.get("limit", [20])[0])
            cursor = int(query.get("cursor", [0])[0])
            items = [job for job in (JOB, LATER_JOB) if job["id"] > cursor]
            page = items[:limit]
            next_cursor = page[-1]["id"] if len(items) > limit else None
            self.respond(200, {"items": page, "next_cursor": next_cursor})
        elif url.path == f"{JOBS}/{JOB_ID}":
            self.respond(200, JOB)
        else:
            self.respond(404, {"error": {"code": "not_found", "message": "Job not found"}})

    def log_message(self, *_args):
        pass


def error(result, expected):
    status, _, body = result
    assert status == expected, f"expected {expected}, got {status}"
    assert body["error"]["code"] and body["error"]["message"]


def read_routes(port, mock):
    assert request(port, SAMPLE)[2] == SUMMARY
    assert request(port, f"{JOBS}/datasets")[::2] == (200, {"items": []})
    assert request(port, f"{JOBS}/{JOB_ID}")[2] == JOB
    code, _, page = request(port, JOBS + "?limit=7&cursor=19")
    assert code == 200 and page == {"items": [JOB, LATER_JOB], "next_cursor": None}
    assert mock.pagination == {"limit": ["7"], "cursor": ["19"]}
    code, _, page = request(port, JOBS + "?limit=1&cursor=19")
    cursor = page["next_cursor"]
    assert code == 200 and page == {"items": [JOB], "next_cursor": cursor}
    assert isinstance(cursor, str) and cursor.startswith("v1.") and len(cursor) == 51
    assert mock.pagination == {"limit": ["1"], "cursor": [str(JOB_ID)]}
    code, _, page = request(port, f"{JOBS}?limit=1&cursor={cursor}")
    assert code == 200 and page == {"items": [LATER_JOB], "next_cursor": None}
    code, _, page = request(port, f"{JOBS}?limit=1&cursor={JOB_ID}")
    assert code == 200 and page == {"items": [LATER_JOB], "next_cursor": None}
    error(request(port, f"{JOBS}/99"), 404)
    code, _, market = request(port, MARKET)
    assert code == 200 and market == mock.market
    code, _, summary = request(port, STATUS)
    assert code == 200 and summary["finance"]["ok"]
    market = summary["market"]
    assert market["state"] == "degraded" and market["source"] == "replay"
    assert not market["fresh"] and market["instrument_count"] == 2 and market["stale_count"] == 1
    print("anonymous list/detail/sample/market/status returned exact data and stale summary", flush=True)


@contextmanager
def web(root, mock, file, max_requests):
    # Inherited admin secrets have no role in the fixture: only a randomly
    # generated local credential in a 0600 file is used.
    (root / "index.html").write_text("<!doctype html>")
    port = reserve_port()
    env = web_env(root, port, mock.server_port, token_file=file, profile={
        "security": {"max_requests": max_requests, "window_ms": 1600},
        "finance": {"request_timeout_ms": 350, "max_retries": 2, "backoff_ms": 10},
    })
    # A raw token env variable must never enable a disabled file-backed write path.
    env["WEB_ADMIN_TOKEN"] = mock.env_decoy
    with (root / "web.log").open("ab") as log:
        proc = subprocess.Popen([os.environ["WEB_BIN"], "serve"], env=env, stderr=log, stdout=log)
        try:
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                assert proc.poll() is None, f"web exited with {proc.returncode}"
                try:
                    if request(port, "/healthz/live")[0] == 200:
                        break
                except (URLError, OSError):
                    pass
                time.sleep(.05)
            else:
                raise AssertionError("web failed to bind")
            yield port
        finally:
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
    assert proc.returncode == 0, proc.returncode


def run(root, mock):
    file = root / "web.env"
    token = secrets.token_urlsafe(32)
    with web(root, mock, file, 10) as port:
        # Middleware must reject before trying to parse the body or idempotency key.
        error(post(port, mock.env_decoy, payload=b"invalid JSON"), 403)
        read_routes(port, mock)
        assert not mock.submissions
    with os.fdopen(os.open(file, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600), "w") as secret:
        secret.write(f"WEB_ADMIN_TOKEN={token}\n")

    with web(root, mock, file, 10) as port:
        key = secrets.token_hex(16)
        lookup = f"{JOBS}/by-key"
        auth = {"Authorization": f"Bearer {token}"}
        error(post(port, key=key), 401)
        error(post(port, mock.env_decoy, key), 401)
        error(request(port, lookup, headers={"Idempotency-Key": key}), 401)
        error(request(port, lookup, headers=auth), 422)
        error(request(port, lookup, headers={**auth, "Idempotency-Key": key}), 404)
        assert not mock.submissions
        code, _, _ = post(port, token)
        assert code in (400, 422) and not mock.submissions, "missing idempotency key was not rejected locally"
        for invalid in [dict(INPUT, params={"unknown": "1.25"}), dict(INPUT, unknown=True)]:
            error(post(port, token, key, payload=json.dumps(invalid).encode()), 422)
        assert not mock.submissions, "invalid request was forwarded to finance"
        assert post(port, token, key)[::2] == (202, {"id": JOB_ID})
        assert mock.submissions == [(JOBS, key, INPUT)]
        error_result = post(port, token, mock.slow_key)
        error(error_result, 504)
        message = error_result[2]["error"]["message"].lower()
        assert "key" in message and ("query" in message or "查询" in message)
        assert [r[1] for r in mock.submissions].count(mock.slow_key) == 1, "POST timeout automatically retried"
        found = request(port, lookup, headers={**auth, "Idempotency-Key": mock.slow_key})
        assert found[::2] == (200, {"id": JOB_ID}) and found[1]["Cache-Control"] == "no-store"
        mock.resume.set()
        assert post(port, token, mock.slow_key)[::2] == (202, {"id": JOB_ID})
        assert mock.submissions[-2:] == [(JOBS, mock.slow_key, INPUT)] * 2
        historical = dict(INPUT, strategy="SmaCrossover", instrument="IBM.XNYS",
                          params={"fast_period": 5, "slow_period": 20, "shares": 10})
        research_key = secrets.token_hex(16)
        assert post(port, token, research_key, payload=json.dumps(historical).encode())[0] == 202
        assert mock.submissions[-1] == (JOBS, research_key, historical)
        print("unconfigured 403; empty/wrong bearer 401; timed-out POST sent once; key lookup and research parameters verified", flush=True)

    with web(root, mock, file, 2) as port:
        old = len(mock.submissions)
        with ThreadPoolExecutor(max_workers=6) as pool:
            calls = [pool.submit(post, port, token, secrets.token_hex(16), address=f"192.0.2.{i}") for i in range(1, 7)]
            replies = [call.result(5) for call in calls]
        assert sorted(r[0] for r in replies) == [202, 202, 429, 429, 429, 429]
        for result in replies:
            if result[0] == 429:
                error(result, 429)
                assert int(result[1]["Retry-After"]) >= 1
        assert len(mock.submissions) == old + 2
        assert request(port, f"{JOBS}/{JOB_ID}")[0] == 200
        time.sleep(1.7)
        assert post(port, token, secrets.token_hex(16))[0] == 202
        print("one token across six forwarded addresses: 2 accepted, 4 rate-limited, window recovered", flush=True)
    assert not mock.auth_forwarded.is_set(), "web leaked its admin authorization header to finance"
    logs = (root / "web.log").read_bytes()
    assert token.encode() not in logs and mock.env_decoy.encode() not in logs, "web logs included a bearer credential"


if __name__ == "__main__":
    with ThreadingHTTPServer(("127.0.0.1", 0), Finance) as mock:
        mock.slow_key = secrets.token_hex(16)
        mock.env_decoy = secrets.token_urlsafe(32)
        mock.resume = threading.Event()
        mock.submissions = []
        mock.lock = threading.Lock()
        mock.auth_forwarded = threading.Event()
        mock.market = {"state": "degraded", "source": "replay", "max_age_seconds": 120,
                       "updated_at": datetime.now(timezone.utc).isoformat(),
                       "instruments": [
                           {"instrument": "SPY.ARCA", "age_seconds": 3, "stale": False},
                           {"instrument": "AAPL.NASDAQ", "age_seconds": 180, "stale": True},
                       ]}
        worker = threading.Thread(target=mock.serve_forever, daemon=True)
        worker.start()
        try:
            with tempfile.TemporaryDirectory(dir="/tmp") as folder:
                run(Path(folder), mock)
        finally:
            mock.resume.set()
            mock.shutdown()
            worker.join()
