"""Offline checks using temporary repositories and fixed synthetic payloads."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
MARKER = "synthetic-canary-wg23"
ZERO = "0" * 40


class DataGuardTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="web-guard-", dir="/tmp")
        self.addCleanup(temporary.cleanup)
        self.repo = Path(temporary.name)
        self.env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                        GIT_AUTHOR_NAME="Web Guard", GIT_COMMITTER_NAME="Web Guard",
                        GIT_AUTHOR_EMAIL="wg@example.test", GIT_COMMITTER_EMAIL="wg@example.test")
        self.git("init", "-q", "-b", "main", "--object-format=sha1")
        for relative in (
            ".data-guard/rules.json", ".data-guard/sources.json",
            ".githooks/pre-commit", ".githooks/pre-push", "scripts/data_guard.py",
            "scripts/install-data-guard-hooks.sh", "scripts/ci-check.sh",
        ):
            target = self.repo / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / relative, target)
        self.git("add", ".")
        self.git("commit", "-qm", "Install rules")
        self.base = self.git("rev-parse", "HEAD").stdout.decode().strip()
        self.git("update-ref", "refs/remotes/origin/main", self.base)
        self.run_command("sh", "scripts/install-data-guard-hooks.sh", check=True)
        self.run_command("sh", "scripts/install-data-guard-hooks.sh", check=True)

    def run_command(self, *args, check=False, input=None):
        return subprocess.run(args, cwd=self.repo, env=self.env, input=input,
                              capture_output=True, check=check)

    def git(self, *args, check=True):
        return self.run_command("git", *args, check=check)

    def write(self, relative, payload):
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(payload.encode() if isinstance(payload, str) else payload)

    def guard(self, *args):
        return self.run_command(sys.executable, "-B", "scripts/data_guard.py", *args)

    def assert_rejected(self, result, reason):
        self.assertNotEqual(result.returncode, 0)
        output = result.stdout + result.stderr
        self.assertIn(reason.encode(), output)
        self.assertNotIn(MARKER.encode(), output)

    def test_pre_commit_rejects_staged_violation_with_clean_worktree(self):
        self.write("quoted.txt", "Authorization: Bearer " + MARKER)
        self.git("add", "quoted.txt")
        self.write("quoted.txt", "Only this worktree copy is clean.")
        self.assert_rejected(self.git("commit", "-qm", "Add quote", check=False),
                             "credential-material")
        self.assertEqual(self.git("rev-parse", "HEAD").stdout.decode().strip(), self.base)

    def test_pre_commit_accepts_reproducible_fixture_and_rejects_drift(self):
        fixture = "tests/api/fixtures/synthetic-quotes.json"
        generator = "tests/synthetic_quotes.py"
        self.write(generator, "print('{\"price\":\"1.2500\",\"quantity\":\"2.0000\",\"exchange_ts\":\"0\"}')\n")
        payload = self.run_command(sys.executable, generator, check=True).stdout
        self.write(fixture, payload)
        self.write(".data-guard/sources.json", json.dumps({
            "version": 1,
            "fixtures": {fixture: {
                "sha256": hashlib.sha256(payload).hexdigest(),
                "generator": generator,
                "seed": "fixed",
                "source": "Fixed decimal strings emitted by tests/synthetic_quotes.py.",
            }},
        }))
        self.git("add", ".")
        self.git("commit", "-qm", "Add synthetic fixture")
        self.write(fixture, payload.replace(b"1.2500", b"1.2501"))
        self.git("add", fixture)
        self.assert_rejected(self.git("commit", "-qm", "Change fixture", check=False),
                             "fixture-digest")
        self.write(fixture, payload)
        self.git("add", fixture)
        self.git("rm", generator)
        self.assert_rejected(self.guard("staged"), "fixture-source-missing")

    def test_range_rejects_removed_violation_and_hides_paths_and_payloads(self):
        path = MARKER + ".txt"
        self.write(path, '{"open":"1.25","high":"2.25","low":"1.00","close":"1.50"}')
        self.git("add", path)
        self.git("-c", "core.hooksPath=" + os.devnull, "commit", "-qm", "Stage the mock violation")
        self.git("rm", path)
        self.git("commit", "-qm", "Remove the mock violation")
        self.assert_rejected(self.guard("range", self.base, "HEAD"), "fixture-source-required")

    def test_pre_push_checks_new_and_existing_refs_and_allows_deletion(self):
        bad = "quotes.txt"
        self.write(bad, "date,price\n2030-01-01,1.0000\n2030-01-02,1.2500\n")
        self.git("add", bad)
        self.git("-c", "core.hooksPath=" + os.devnull, "commit", "-qm", "Add synthetic export")
        tip = self.git("rev-parse", "HEAD").stdout.decode().strip()
        prefix = f"refs/heads/main {tip} refs/heads/main "
        for old in (self.base, ZERO):
            result = self.run_command(".git/hooks/pre-push", "origin", "local-only",
                                      input=(prefix + old + "\n").encode())
            self.assert_rejected(result, "fixture-source-required")
        self.run_command(".git/hooks/pre-push", "origin", "local-only", check=True,
                         input=f"(delete) {ZERO} refs/heads/main {tip}\n".encode())

    def test_web_exports_generated_samples_and_disguised_formats_are_rejected(self):
        credential = json.dumps({"api_token": MARKER})
        cases = (
            ("notes.txt", credential, "credential-material"),
            ("transcript.txt", "Cookie: __Host-web_admin_session=" + MARKER, "credential-material"),
            ("key.txt", "-----BEGIN " + "PRIVATE KEY-----\n" + MARKER, "credential-material"),
            ("notes.txt", json.dumps({"method": "PUT", "headers": {}, "body": MARKER}),
             "fixture-source-required"),
            ("notes.txt", json.dumps({"timestamp": "0", "level": "WARN", "fields": {"message": MARKER}}),
             "log-export"),
            ("trace.txt", "Traceback (most recent call last):\n" + MARKER, "log-export"),
            ("notes.txt", '<?xml version="1.0"?><testsuites>' + MARKER, "test-report"),
            ("output/test-results/run.txt", MARKER, "blocked-path"),
            ("notes.txt", json.dumps({"testResults": [MARKER], "numFailedTests": 1}), "test-report"),
            ("openapi/fixture.json", json.dumps({"schema": {"example": MARKER}}),
             "fixture-source-required"),
            ("crates/finance-client/examples/query.rs", "// " + MARKER, "fixture-source-required"),
            ("crates/finance-client/examples/notes.txt", credential, "credential-material"),
            ("archive.txt", b"PK\x03\x04", "blocked-magic"),
            ("screen.txt", b"\x89PNG\r\n\x1a\n", "blocked-magic"),
            ("chart.txt", b"PAR1", "blocked-magic"),
            ("store.txt", b"SQLite format 3\x00", "blocked-magic"),
            ("quotes.CSV", MARKER, "fixture-source-required"),
            ("big.txt", b"x" * (256 * 1024 + 1), "oversize"),
            (".github/workflows/example.yml", "steps:\n  - uses: actions/" + "upload-artifact@v4",
             "artifact-upload"),
        )
        for path, content, reason in cases:
            with self.subTest(path=path, reason=reason):
                self.write(path, content)
                self.git("add", path)
                self.assert_rejected(self.guard("staged"), reason)
                self.git("rm", "-f", path)

    def test_invalid_rule_versions_and_duplicate_keys_fail_closed(self):
        for text in ('{"version":2}', '{"version":1,"version":1}'):
            with self.subTest(text=text):
                self.write(".data-guard/rules.json", text)
                self.git("add", ".data-guard/rules.json")
                self.assert_rejected(self.guard("staged"), "invalid-rules")

    def test_pre_commit_recognizes_market_data_inside_text_wrappers(self):
        columns = ("open", "high", "low", "close")
        values = ("1.2301", "1.7000", "1.0100", "1.4000")
        quote = json.dumps(dict(zip(columns, values)))
        table = "\n".join(f"| {' | '.join(row)} |" for row in (columns, ("---",) * 4, values))
        cases = (
            ("transcript.txt", "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n" + quote),
            ("transcript.rs", "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n" + quote),
            ("failure.txt", "running 1 test\n---- sample stdout ----\nleft: " + json.dumps(quote)),
            ("notes.md", "Synthetic output\n" + quote),
            ("table.md", "Synthetic output\n" + table),
            ("quote.md", "Quote snapshot\nbid_price: '1.2301'\naskPrice: '1.2400'\n"),
            ("trade.txt", 'Output: {"price":"1.2301","size":"2.00","side":"buy"}'),
            ("book.txt", 'Probe: {"bids":[["1.2301","2.00"]],"asks":[["1.24","3.00"]]}'),
        )
        for path, payload in cases:
            with self.subTest(path=path):
                self.write(path, payload)
                self.git("add", path)
                result = self.git("commit", "-qm", "Add wrapped payload", check=False)
                self.assert_rejected(result, "fixture-source-required")
                self.assertNotIn(b"1.2301", result.stdout + result.stderr)
                self.assertEqual(self.git("rev-parse", "HEAD").stdout.decode().strip(), self.base)
                self.git("rm", "-f", path)

    def test_multiline_credential_and_credential_paths_are_rejected_without_echo(self):
        payload = '{\n "api_token":\n ' + json.dumps(MARKER) + '\n}\n'
        self.write("transcript.txt", "Synthetic response\n" + payload)
        self.git("add", "transcript.txt")
        self.assert_rejected(self.git("commit", "-qm", "Add response", check=False),
                             "credential-material")
        self.git("rm", "-f", "transcript.txt")
        for path in (
            "notes/api_token=" + MARKER + ".txt",
            "notes/Authorization%3A%20Bearer%20" + MARKER + ".txt",
            "notes/Bearer/" + MARKER + ".txt",
        ):
            with self.subTest(shape=path.rsplit("/", 1)[0]):
                self.write(path, "clean content")
                self.git("add", path)
                result = self.git("commit", "-qm", "Add path", check=False)
                self.assert_rejected(result, "credential-material <redacted-path>")
                self.git("rm", "-f", path)

    def test_managed_paths_require_sources_and_accept_only_registered_bytes(self):
        path = "tests/local/fixtures/snapshot.http"
        generator = "tests/gen_capture.py"
        self.write(generator, 'print("HTTP/1.1 200 OK\\n\\n" + \'{"bid":"1.00","ask":"2.00"}\')\n')
        data = self.run_command(sys.executable, generator, check=True).stdout
        for payload in (data, b"plain synthetic capture"):
            self.write(path, payload)
            self.git("add", path)
            self.assert_rejected(self.guard("staged"), "fixture-source-required")
        self.write(path, data)
        self.write(".data-guard/sources.json", json.dumps({
            "version": 1,
            "fixtures": {path: {
                "sha256": hashlib.sha256(data).hexdigest(),
                "generator": generator,
                "seed": "fixed",
                "source": "Fixed local HTTP response from tests/gen_capture.py.",
            }},
        }))
        self.git("add", ".")
        self.git("commit", "-qm", "Add registered replay")
        self.write(path, data + b"\n")
        self.git("add", path)
        self.assert_rejected(self.guard("staged"), "fixture-digest")

    def test_ci_check_hides_success_and_failure_output(self):
        for status in (0, 5):
            with self.subTest(status=status):
                script = "import sys; print(sys.argv[1]); print(sys.argv[1], file=sys.stderr); sys.exit(int(sys.argv[2]))"
                result = self.run_command("sh", "scripts/ci-check.sh", "fixture", sys.executable,
                                          "-c", script, MARKER, str(status))
                self.assertEqual(result.returncode, status)
                self.assertNotIn(MARKER.encode(), result.stdout + result.stderr)
                self.assertIn(b"passed" if status == 0 else b"failed (exit 5)",
                              result.stdout + result.stderr)
