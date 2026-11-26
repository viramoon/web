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

    def stage_relaxed_rules(self):
        path = ".data-guard/rules.json"
        rules = json.loads((self.repo / path).read_text())
        original_limit = rules["max_blob_bytes"]
        rules["max_blob_bytes"] += 1
        rules["blocked_path_components"].remove("logs")
        self.write(path, json.dumps(rules))
        self.git("add", path)
        return original_limit

    def stage_rule_violations(self, original_limit):
        # These are plain synthetic documents with otherwise allowed file types.
        oversized = "docs/oversized.md"
        blocked = "docs/logs/fixture.md"
        self.write(oversized, MARKER.ljust(original_limit + 1, "x"))
        self.write(blocked, MARKER)
        self.git("add", oversized, blocked)

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

    def test_pre_commit_uses_head_rules_instead_of_staged_relaxation(self):
        self.stage_rule_violations(self.stage_relaxed_rules())
        result = self.git("commit", "-qm", "Add samples while relaxing policy", check=False)
        self.assert_rejected(result, "oversize")
        self.assert_rejected(result, "blocked-path")
        self.assertEqual(self.git("rev-parse", "HEAD").stdout.decode().strip(), self.base)

    def test_pre_push_uses_remote_default_rules_for_new_and_existing_refs(self):
        original_limit = self.stage_relaxed_rules()
        self.git("commit", "-qm", "Propose schema-valid rule changes")
        relaxed = self.git("rev-parse", "HEAD").stdout.decode().strip()
        self.stage_rule_violations(original_limit)
        self.git("commit", "-qm", "Add files allowed by the local branch policy")
        tip = self.git("rev-parse", "HEAD").stdout.decode().strip()
        for old in (ZERO, relaxed):
            with self.subTest(new_ref=old == ZERO):
                refs = f"refs/heads/topic {tip} refs/heads/topic {old}\n".encode()
                result = self.run_command(".git/hooks/pre-push", "origin", "local-only", input=refs)
                self.assert_rejected(result, "oversize")
                self.assert_rejected(result, "blocked-path")

    def test_ci_range_uses_pr_base_rules_instead_of_candidate_relaxation(self):
        self.stage_rule_violations(self.stage_relaxed_rules())
        self.git("-c", "core.hooksPath=" + os.devnull, "commit", "-qm", "Mock unchecked proposal")
        result = self.guard("range", self.base, "HEAD")
        self.assert_rejected(result, "oversize")
        self.assert_rejected(result, "blocked-path")

    def test_ci_range_validates_rule_schema_in_intermediate_commits(self):
        path = ".data-guard/rules.json"
        original = (self.repo / path).read_bytes()
        self.write(path, '{"version":2}')
        self.git("add", path)
        self.git("-c", "core.hooksPath=" + os.devnull, "commit", "-qm", "Mock invalid policy")
        self.write(path, original)
        self.git("add", path)
        self.git("-c", "core.hooksPath=" + os.devnull, "commit", "-qm", "Restore valid policy")
        self.assert_rejected(self.guard("range", self.base, "HEAD"), "invalid-rules")

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

    def test_path_and_type_allowlist_defaults_to_verified_sources(self):
        allowed = {
            "README.md": "# Synthetic repo",
            "src/nested/lib.rs": "pub fn fixture() {}",
            "tests/nested/probe.py": "assert 1 == 1",
            "docs/nested/guide.md": "# Synthetic guide",
            "migrations/20990101000000_fixture.sql": "SELECT 1",
            "openapi/contract.json": '{"openapi":"3.1.0"}',
            "crates/finance-client/src/nested/query.rs": "pub fn query() {}",
            ".github/workflows/check.yml": "name: Fixture check",
        }
        for path, content in allowed.items():
            self.write(path, content)
        self.git("add", ".")
        self.git("commit", "-qm", "Add ordinary repository files")

        payload = "Plain synthetic text"
        for path in ("notes.txt", "notes.md", "src/notes.txt", "docs/helper.rs",
                     "ops/task.py", "migrations/nested/fixture.sql", "tests/probe.jsonl"):
            with self.subTest(path=path):
                self.write(path, payload)
                self.git("add", path)
                self.assert_rejected(self.guard("staged"), "fixture-source-required")
                self.git("rm", "-f", path)

        path = "notes.txt"
        generator = "tests/gen_notes.py"
        self.write(generator, f"print({payload!r}, end='')")
        generated = self.run_command(sys.executable, generator, check=True).stdout
        self.write(path, generated)
        self.write(".data-guard/sources.json", json.dumps({
            "version": 1,
            "fixtures": {path: {
                "sha256": hashlib.sha256(generated).hexdigest(),
                "generator": generator,
                "seed": "fixed",
                "source": "Fixed text emitted by tests/gen_notes.py.",
            }},
        }))
        self.git("add", ".")
        self.git("commit", "-qm", "Add registered text outside the allowlist")
        result = self.guard("range", self.base, "HEAD")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_native_reports_and_embedded_logs_are_hard_rejected(self):
        cargo = "\n".join((
            "   Compiling smoke v0.0.0",
            "     Running unittests src/lib.rs (target/debug/deps/smoke)",
            "running 1 test",
            f"test {MARKER} ... ok",
            "test result: ok. 1 passed; 0 failed; 0 ignored",
        ))
        libtest = "\n".join(map(json.dumps, (
            dict(type="suite", event="started", test_count=1),
            dict(type="test", name=MARKER, event="ok"),
            dict(event="ok", type="suite", passed=1, failed=0),
        )))
        log = json.dumps({
            "timestamp": "2030-01-01T00:00:00Z",
            "level": "WARN",
            "fields": {"message": MARKER},
        }, indent=2)
        fence = chr(96) * 3
        cases = (
            ("notes.txt", cargo, "test-report"),
            ("docs/guide.md", "An attached result\n" + cargo, "test-report"),
            ("notes.txt", libtest, "test-report"),
            ("tests/fixture.jsonl", libtest, "test-report"),
            ("docs/guide.md", f"A pasted log\n{fence}json\n{log}\n{fence}", "log-export"),
            ("docs/guide.md", "\n".join("> " + line for line in log.splitlines()), "log-export"),
            ("docs/guide.md", "Run record: " + json.dumps({"raw": log}), "log-export"),
            ("docs/guide.md", "> 2030-01-01T00:00:00Z INFO web: " + MARKER, "log-export"),
            ("docs/guide.md", "2030-01-01 00:00:00.123 | WARN | " + MARKER, "log-export"),
            ("docs/guide.md", "Captured request\nGET /synthetic HTTP/1.1\nHost: loopback\n",
             "fixture-source-required"),
        )
        for path, payload, reason in cases:
            with self.subTest(path=path, reason=reason):
                self.write(path, payload)
                self.git("add", path)
                self.assert_rejected(self.git("commit", "-qm", "Add captured output", check=False),
                                     reason)
                self.assertEqual(self.git("rev-parse", "HEAD").stdout.decode().strip(), self.base)
                self.git("rm", "-f", path)

        for path, payload, reason in (
            ("tests/fixture.jsonl", libtest, "test-report"),
            ("docs/guide.md", log, "log-export"),
        ):
            with self.subTest(registered=path):
                generator = "tests/gen_output.py"
                self.write(generator, f"import sys\nsys.stdout.buffer.write(bytes.fromhex({payload.encode().hex()!r}))")
                data = self.run_command(sys.executable, generator, check=True).stdout
                self.write(path, data)
                self.write(".data-guard/sources.json", json.dumps({
                    "version": 1,
                    "fixtures": {path: {
                        "sha256": hashlib.sha256(data).hexdigest(),
                        "generator": generator,
                        "seed": "fixed",
                        "source": "Fixed synthetic output emitted by tests/gen_output.py.",
                    }},
                }))
                self.git("add", ".")
                self.assert_rejected(self.guard("staged"), reason)
                self.git("rm", "-f", path)

    def test_w3_code_rows_are_allowed_but_delimited_data_still_needs_a_source(self):
        cases = {
            "src/notifications/worker.rs": (
                "fn configure() {\n    set_limit(\n        1,\n        1000,\n    );\n}\n"
            ),
            "tests/api/notifications_worker.rs": (
                "fn rules(rule: u64) {\n    let _ = [\n"
                "        (rule, 1, false),\n        (rule, 2, true),\n    ];\n}\n"
            ),
            "src/notifications/generic.rs": (
                "fn set_limits<T>(_: T, _: T) {}\nfn configure() {\n"
                "    set_limits::<u32>(\n        1,\n        1000,\n    );\n}\n"
            ),
            "src/notifications/signed.rs": (
                "fn set_limits(_: i32, _: i32) {}\nfn configure() {\n"
                "    set_limits(\n        -1,\n        -2,\n    );\n}\n"
            ),
        }
        for path, content in cases.items():
            self.write(path, content)
        self.git("add", ".")
        rejected = self.guard("--show-paths", "staged")
        self.assert_rejected(rejected, "fixture-source-required")
        for path in cases:
            self.assertIn(f"fixture-source-required {ascii(path)}".encode(), rejected.stderr)

        generator = "tests/gen_w3_sources.py"
        encoded = {path: content.encode().hex() for path, content in cases.items()}
        self.write(generator, f"import sys\nfiles = {encoded!r}\n"
                   "sys.stdout.buffer.write(bytes.fromhex(files[sys.argv[1]]))\n")
        sources = {}
        for path, content in cases.items():
            generated = self.run_command(sys.executable, generator, path, check=True).stdout
            self.assertEqual(generated, content.encode())
            sources[path] = {
                "sha256": hashlib.sha256(generated).hexdigest(),
                "generator": generator,
                "seed": "fixed",
                "source": "Fixed Rust source emitted by tests/gen_w3_sources.py for this path.",
            }
        self.write(".data-guard/sources.json", json.dumps({"version": 1, "fixtures": sources}))
        self.git("add", ".")
        self.git("commit", "-qm", "Add registered W3-style source")

        path = "src/notifications/capture.rs"
        for table in ("date,price\n2030-01-01,1.25\n", "1,2,\n3,4,\n"):
            with self.subTest(table=table):
                self.write(path, table)
                self.git("add", path)
                self.assert_rejected(self.guard("staged"), "fixture-source-required")
                self.git("rm", "-f", path)

    def test_reviewed_market_source_requires_the_exact_path_and_bytes(self):
        path = "src/notifications/worker.rs"
        original = (ROOT / path).read_bytes()
        self.write(path, original)
        self.git("add", path)
        self.git("commit", "-qm", "Add W3 source by reviewed digest")
        self.write(path, original + b"\n// changed\n")
        self.git("add", path)
        self.assert_rejected(self.guard("staged"), "fixture-source-required")

        self.write(path, original)
        self.git("add", path)
        copy = "src/copied_notification_worker.rs"
        self.write(copy, original)
        self.git("add", copy)
        self.assert_rejected(self.guard("staged"), "fixture-source-required")

    def test_ci_postgres_credentials_are_pinned_to_the_service_workflow(self):
        path = ".github/workflows/service.yml"
        fixture = (ROOT / path).read_text()
        self.write(path, fixture)
        self.git("add", path)
        self.git("commit", "-qm", "Restore the approved CI database settings")
        for key in ("POSTGRES_PASSWORD", "DATABASE_URL"):
            with self.subTest(key=key):
                lines = fixture.splitlines(keepends=True)
                changed = [line.replace("local-ci-only", MARKER) if line.lstrip().startswith(key + ": ")
                           else line for line in lines]
                self.assertNotEqual(changed, lines)
                self.write(path, "".join(changed))
                self.git("add", path)
                self.assert_rejected(self.guard("staged"), "credential-material")
        self.write(path, fixture)
        self.git("add", path)
        copy = ".github/workflows/copy.yml"
        self.write(copy, fixture)
        self.git("add", copy)
        self.assert_rejected(self.guard("staged"), "credential-material")

    def test_reviewed_w3_credentials_require_the_exact_path_and_source_bytes(self):
        path = "src/notifications/channel/tests.rs"
        fixture = (ROOT / path).read_bytes()
        self.write(path, fixture)
        self.git("add", path)
        self.git("commit", "-qm", "Add the reviewed W3 test source")
        self.write(path, fixture + b"\n// modified\n")
        self.git("add", path)
        self.assert_rejected(self.guard("staged"), "credential-material")

        self.write(path, fixture)
        self.git("add", path)
        copy = "tests/copied_smtp.rs"
        self.write(copy, fixture)
        self.git("add", copy)
        self.assert_rejected(self.guard("staged"), "credential-material")

    def test_headerless_csv_with_empty_second_column_is_rejected_in_rust_paths(self):
        path = "src/notifications/capture.rs"
        for data in (
            "1.2500,\n1.3000,\n",
            "1,\n2,\n",
            "fn configure() {}\n\n    1,\n    2,\n",
            "fn configure() {\n    set_limit(\n        1,\n        2,\n",
            "fn configure() {\n    set_limit(\n        1.2500,\n        1.3000,\n    );\n}\n",
        ):
            with self.subTest(data=data):
                self.write(path, data)
                self.git("add", path)
                result = self.git("commit", "-qm", "Add unregistered CSV", check=False)
                self.assert_rejected(result, "fixture-source-required")
                self.assertNotIn(b"1.2500", result.stdout + result.stderr)
                self.assertEqual(self.git("rev-parse", "HEAD").stdout.decode().strip(), self.base)
                self.git("rm", "-f", path)

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
