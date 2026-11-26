#!/usr/bin/env python3
"""Exercise the production web executable and generated client with a local finance HTTP server."""
import json
import os
from collections import deque
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import subprocess
import tempfile
import threading
import time
from urllib.error import URLError

from core_http import get, reserve_port
from http_fixture import web_env


SAMPLE = "/api/v1/backtests/sample"
STATUS = "/api/v1/system/status"
READY = "/healthz/ready"
MARKET = "/api/v1/market/status"
REQUEST_MS = 1500
OPEN_MS = 1800
FAILURES = 2
RETRIES = 2
PNL = "12345678901234567890.12345678"
SUMMARY = {
    "run_id": "run-precision",
    "strategy": "demo",
    "instrument": "SPY.ARCA",
    "start": "2026-11-02T14:30:00Z",
    "end": "2026-11-02T15:30:00Z",
    "total_orders": 7,
    "total_positions": 3,
    "pnl": PNL,
    "currency": "USD",
    "generated_at": "2026-11-02T15:31:00Z",
}


@dataclass
class Reply:
    code: int
    body: bytes
    release: threading.Event | None = None


def reply(code, value, release=None):
    return Reply(code, json.dumps(value).encode(), release)


SUCCESS = reply(200, SUMMARY)
NOT_FOUND = reply(404, {"error": {"code": "not_found", "message": "No sample"}})
UPSTREAM_ERROR = reply(500, {"error": {"code": "internal_error", "message": "Temporary failure"}})


class FinanceHTTP(ThreadingHTTPServer):
    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.paths = []
        self.changed = threading.Condition()
        self.replies = deque([SUCCESS])

    def plan(self, *replies):
        with self.changed:
            self.replies = deque(replies)

    def take(self, path):
        with self.changed:
            self.paths.append(path)
            self.changed.notify_all()
            if path == READY:
                return reply(200, {"status": "ready", "checks": []})
            if path == MARKET:
                return reply(200, {"state": "ready", "source": "replay", "instruments": [{
                    "instrument": "SPY.ARCA", "age_seconds": 0, "stale": False,
                }]})
            if path == SAMPLE:
                return self.replies.popleft() if len(self.replies) > 1 else self.replies[0]
            return reply(404, {"error": {"code": "unexpected_path", "message": path}})

    def count(self, path=SAMPLE):
        with self.changed:
            return self.paths.count(path)

    def wait_sample(self, count):
        with self.changed:
            assert self.changed.wait_for(lambda: self.paths.count(SAMPLE) >= count, timeout=4), self.paths


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        response = self.server.take(self.path)
        if response.release is not None and not response.release.wait(15):
            return
        try:
            self.send_response(response.code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(response.body)))
            self.end_headers()
            self.wfile.write(response.body)
        except (BrokenPipeError, ConnectionResetError):
            # The deadline test intentionally closes all upstream attempts.
            pass

    def log_message(self, *_args):
        pass


def sample(port):
    code, headers, data = get(port, SAMPLE)
    assert headers.get_content_type() == "application/json", (code, data)
    return code, json.loads(data)


def error(result, statuses, expected=None):
    code, body = result
    assert code in statuses and isinstance(body, dict), result
    detail = body["error"]
    assert isinstance(detail["message"], str) and detail["message"], result
    assert isinstance(detail["code"], str) and detail["code"], result
    if expected:
        assert detail["code"] == expected, result


def finance_status(port, state):
    code, _, body = get(port, STATUS)
    finance = json.loads(body)["finance"]
    assert code == 200 and finance["circuit"] == state, (code, finance)
    return finance


def exercise(port, mock):
    assert finance_status(port, "closed")["ok"]
    assert mock.count(READY) == 1

    assert sample(port) == (200, SUMMARY)
    assert mock.count() == 1
    before = mock.count()
    mock.plan(NOT_FOUND)
    error(sample(port), (404,))
    assert mock.count() == before + 1, "404 was retried"
    print("200 sample equals finance including pnl", PNL, "and 404 took one request", flush=True)

    mock.plan(UPSTREAM_ERROR, UPSTREAM_ERROR, SUCCESS)
    before = mock.count()
    assert sample(port) == (200, SUMMARY)
    assert mock.count() == before + 3, "two temporary errors must recover on the last allowed retry"

    # Construct a raw JSON number without passing the amount through a Python float.
    numeric_json = SUCCESS.body.replace(json.dumps(PNL).encode(), PNL.encode())
    assert numeric_json != SUCCESS.body
    mock.plan(Reply(200, numeric_json))
    error(sample(port), (502, 503))
    mock.plan(SUCCESS)
    assert sample(port) == (200, SUMMARY)
    print("5xx -> 5xx -> 200; upstream numeric pnl rejected", flush=True)

    release = threading.Event()
    mock.plan(reply(200, SUMMARY, release))
    before = mock.count()
    try:
        error(sample(port), (504,), "finance_timeout")
        assert mock.count() - before == RETRIES + 1, "wrong number of timeout attempts"
    finally:
        release.set()
    mock.plan(SUCCESS)
    assert sample(port) == (200, SUMMARY)
    print("timeout -> 504 finance_timeout after", RETRIES + 1, "attempts", flush=True)

    mock.plan(UPSTREAM_ERROR)
    for _ in range(FAILURES):
        before = mock.count()
        error(sample(port), (502, 503))
        assert mock.count() - before == RETRIES + 1, "circuit opened at the wrong logical failure count"
    before = mock.count()
    probes = mock.count(READY)
    market_reads = mock.count(MARKET)
    start = time.monotonic()
    error(sample(port), (503,), "finance_unavailable")
    assert time.monotonic() - start < REQUEST_MS / 1000, "open circuit waited for upstream"
    assert not finance_status(port, "open")["ok"]
    assert (mock.count(), mock.count(READY), mock.count(MARKET)) == (before, probes, market_reads), "open circuit reached finance"

    # Hold one active half-open call while a burst of peers arrives. None of them
    # may start another upstream attempt, consume a retry, or wait for the probe.
    time.sleep((OPEN_MS + 100) / 1000)
    gate = threading.Event()
    mock.plan(reply(200, SUMMARY, gate))
    with ThreadPoolExecutor(max_workers=9) as clients:
        first = clients.submit(sample, port)
        try:
            mock.wait_sample(before + 1)
            peers = [clients.submit(sample, port) for _ in range(8)]
            for future in peers:
                error(future.result(timeout=REQUEST_MS / 1000), (503,), "finance_unavailable")
            assert not finance_status(port, "half_open")["ok"]
            assert not first.done(), "other calls did not return while the probe was held"
            assert (mock.count(), mock.count(READY), mock.count(MARKET)) == (before + 1, probes, market_reads), (
                "concurrent requests amplified the half-open probe"
            )
        finally:
            gate.set()
        assert first.result(timeout=4) == (200, SUMMARY)
    assert finance_status(port, "closed")["ok"]
    mock.plan(SUCCESS)
    assert sample(port) == (200, SUMMARY)
    assert mock.count() == before + 2
    assert set(mock.paths) == {SAMPLE, READY, MARKET}, mock.paths
    print("circuit open -> 503 without upstream I/O; half-open 1 probe, 8 rejections; recovered 200", flush=True)


def run(root, mock):
    static = root / "dist"
    static.mkdir()
    (static / "index.html").write_text("<!doctype html>")
    port = reserve_port()
    env = web_env(root, port, mock.server_port, static=static, profile={"finance": {
        "request_timeout_ms": REQUEST_MS, "connect_timeout_ms": 500, "max_retries": RETRIES,
        "backoff_ms": 10, "circuit_failure_threshold": FAILURES, "circuit_open_ms": OPEN_MS,
    }})
    proc = subprocess.Popen([os.environ["WEB_BIN"], "serve"], env=env)
    try:
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            assert proc.poll() is None, proc.returncode
            try:
                if get(port, "/healthz/live")[0] == 200:
                    break
            except (URLError, OSError):
                pass
            time.sleep(.05)
        else:
            raise AssertionError("web did not start")
        exercise(port, mock)
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
    assert proc.returncode == 0, proc.returncode


if __name__ == "__main__":
    with FinanceHTTP() as mock:
        worker = threading.Thread(target=mock.serve_forever, daemon=True)
        worker.start()
        try:
            with tempfile.TemporaryDirectory(dir="/tmp") as folder:
                run(Path(folder), mock)
        finally:
            mock.shutdown()
            worker.join()
