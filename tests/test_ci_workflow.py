"""Offline structure checks for the workflows that run web verification."""

import hashlib
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest

import yaml


WORKFLOWS = Path(__file__).resolve().parents[1] / ".github/workflows"
TEST_PASS = "local-ci-only"
CI_DATABASE_URL = "postgresql://" + f"postgres:{TEST_PASS}@127.0.0.1:5432/web_ci"


def load(name):
    # BaseLoader preserves the GitHub Actions key 'on' as a string.
    return yaml.load((WORKFLOWS / name).read_text(encoding="utf-8"), Loader=yaml.BaseLoader)


class WorkflowTests(unittest.TestCase):
    def test_service_uses_documented_postgres_port_mapping_and_unambiguous_env(self):
        workflow = load("service.yml")
        self.assertIn("pull_request", workflow["on"])
        core = workflow["jobs"]["core"]
        self.assertNotIn("uses", core)
        self.assertEqual(core["needs"], "data-guard")
        postgres = core["services"]["postgres"]
        self.assertEqual(postgres["ports"], ["5432:5432"])
        self.assertEqual(postgres["env"], {"POSTGRES_DB": "web_ci", "POSTGRES_PASSWORD": TEST_PASS})
        self.assertEqual(core["env"]["DATABASE_URL"], CI_DATABASE_URL)
        migrate = next(step for step in core["steps"] if "sqlx migrate run" in step.get("run", ""))
        self.assertNotIn("TEST_PG_PORT", migrate.get("env", {}))
        self.assertNotIn("DATABASE_URL=", migrate["run"])
        self.assertIn('"$GITHUB_ENV"', migrate["run"])
        for scope in (workflow, core, *core["steps"], *core["services"].values()):
            keys = list(scope.get("env", {}))
            self.assertEqual(len(keys), len({key.casefold() for key in keys}))

    def test_core_checks_incoming_git_objects_before_the_complete_service_suite(self):
        jobs = load("service.yml")["jobs"]
        self.assertEqual(jobs["core"]["needs"], "data-guard")
        self.assertNotIn("uses", jobs["data-guard"])
        self.assertIn("runs-on", jobs["data-guard"])
        steps = jobs["data-guard"]["steps"]
        self.assertEqual(steps[0]["uses"], "actions/checkout@v4")
        self.assertEqual(steps[0]["with"]["fetch-depth"], "0")
        self.assertEqual(steps[0]["with"]["persist-credentials"], "false")
        self.assertEqual(steps[1]["run"], 'sh scripts/ci-data-guard.sh "$BASE_SHA"')
        self.assertEqual(steps[1]["env"]["BASE_SHA"], "${{ github.event.pull_request.base.sha || github.event.before }}")
        commands = "\n".join(step.get("run", "") for step in jobs["core"]["steps"])
        checks = (
            "cargo sqlx migrate run",
            "cargo fmt --all -- --check",
            "cargo clippy --workspace --locked --all-targets -- -D warnings",
            "cargo test --workspace --locked",
            "cargo sqlx prepare --check -- --all-targets",
            "python3 -B tests/api/core_http.py",
            "python3 -B tests/api/finance_resilience.py",
            "python3 -B tests/api/finance_writes.py",
            "target/debug/web export-openapi",
        )
        self.assertEqual(commands.count("target/debug/web export-openapi"), 2)
        positions = [commands.find(check) for check in checks]
        self.assertGreaterEqual(min(positions), 0)
        self.assertEqual(positions, sorted(positions))

    def test_migration_uses_the_authenticated_service_url(self):
        core = load("service.yml")["jobs"]["core"]
        migrate = next(step["run"] for step in core["steps"] if "sqlx migrate run" in step.get("run", ""))
        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            root = Path(directory)
            cargo = root / "cargo"
            cargo.write_text(
                '#!/bin/sh\nset -eu\ntest "$*" = "sqlx migrate run"\n'
                'test "$DATABASE_URL" = "$EXPECTED_DATABASE_URL"\n'
                'printf "%s\n" "$DATABASE_URL" > "$RUNNER_TEMP/migrated"\n'
            )
            cargo.chmod(0o700)
            env_file = root / "job.env"
            env = {"PATH": str(root) + os.pathsep + os.defpath, "GITHUB_ENV": str(env_file),
                   "RUNNER_TEMP": str(root), "EXPECTED_DATABASE_URL": CI_DATABASE_URL}
            result = subprocess.run(["sh", "-eu", "-c", migrate], cwd=WORKFLOWS.parents[1],
                                    env=env, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((root / "migrated").exists())
            env.update(core["env"])
            result = subprocess.run(["sh", "-eu", "-c", migrate], cwd=WORKFLOWS.parents[1],
                                    env=env, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            url = (root / "migrated").read_text().strip()
            self.assertEqual(url, CI_DATABASE_URL)
            self.assertIn("WEB_ADMIN_TOKEN_FILE=" + str(root / "web-ci-absent-admin.env"),
                          env_file.read_text().splitlines())
            self.assertNotIn("DATABASE_URL=", env_file.read_text())

    def test_reusable_guard_for_docs_and_finance_has_compatible_permissions(self):
        guard = load("data-guard.yml")
        self.assertIsInstance(guard["on"], dict)
        self.assertIn("workflow_call", guard["on"])
        self.assertEqual(guard["permissions"], {"contents": "read"})
        scan = guard["jobs"]["scan"]
        self.assertEqual(scan["steps"][0]["with"]["fetch-depth"], "0")
        self.assertEqual(scan["steps"][1]["run"], 'sh scripts/ci-data-guard.sh "$BASE_SHA"')
        for name, job in (("docs-check.yml", "docs"), ("finance-client.yml", "regenerate")):
            with self.subTest(workflow=name):
                caller = load(name)
                self.assertEqual(caller["permissions"], guard["permissions"])
                self.assertEqual(caller["jobs"]["data-guard"]["uses"], "./.github/workflows/data-guard.yml")
                self.assertEqual(caller["jobs"][job]["needs"], "data-guard")


class FinanceGeneratorWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.root = WORKFLOWS.parents[1]
        self.steps = load("finance-client.yml")["jobs"]["regenerate"]["steps"]
        self.pin = next(step for step in self.steps if step.get("id") == "generator-pin")
        self.cache = next(step for step in self.steps if step.get("id") == "generator-cache")
        self.verify = next(step for step in self.steps if step.get("name") ==
                           "Download or verify the pinned OpenAPI Generator JAR")
        self.generate = next(step for step in self.steps if "gen-finance-client.sh --check" in step.get("run", ""))

    def run_bash_step(self, step, env):
        self.assertEqual(step["shell"], "bash")
        return subprocess.run(
            ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", step["run"]],
            cwd=self.root, env=env, capture_output=True, text=True, timeout=10,
        )

    def test_finance_generator_cache_is_keyed_by_version_and_sha256(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            output = Path(directory) / "outputs"
            result = self.run_bash_step(
                self.pin, {"PATH": os.defpath, "RUNNER_TEMP": directory, "GITHUB_OUTPUT": str(output)},
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            pins = dict(line.split("=", 1) for line in output.read_text().splitlines())
            script = (self.root / "scripts/gen-finance-client.sh").read_text()
            version = re.search(r"^version=([\d.]+)$", script, re.MULTILINE)[1]
            checksum = re.search(r"^checksum=([0-9a-f]{64})$", script, re.MULTILINE)[1]
            jar = f"openapi-generator-cli-{version}.jar"
            self.assertEqual(pins, {
                "version": version,
                "sha256": checksum,
                "url": "https://repo.maven.apache.org/maven2/org/openapitools/"
                       f"openapi-generator-cli/{version}/{jar}",
                "jar": str(Path(directory) / jar),
            })
        self.assertEqual(self.cache["uses"], "actions/cache@v4")
        self.assertEqual(self.cache["with"], {
            "path": "${{ steps.generator-pin.outputs.jar }}",
            "key": "web-openapi-generator-${{ steps.generator-pin.outputs.version }}-"
                   "${{ steps.generator-pin.outputs.sha256 }}",
        })

    def test_finance_generator_verifies_jar_before_running_the_existing_check(self):
        positions = [self.steps.index(step) for step in (self.pin, self.cache, self.verify, self.generate)]
        self.assertEqual(positions, sorted(positions))
        for step in (self.verify, self.generate):
            self.assertNotIn("if", step)
            self.assertNotIn("continue-on-error", step)
        self.assertEqual(self.verify["env"], {
            "CACHE_HIT": "${{ steps.generator-cache.outputs.cache-hit }}",
            "GENERATOR_URL": "${{ steps.generator-pin.outputs.url }}",
            "GENERATOR_SHA256": "${{ steps.generator-pin.outputs.sha256 }}",
            "OPENAPI_GENERATOR_JAR": "${{ steps.generator-pin.outputs.jar }}",
        })
        self.assertIn("sha256sum --check", self.verify["run"])
        self.assertEqual(self.generate["env"], {"OPENAPI_GENERATOR_JAR": "${{ steps.generator-pin.outputs.jar }}"})
        self.assertEqual(self.generate["run"].strip(),
                         "sh scripts/ci-check.sh finance-client-generation scripts/gen-finance-client.sh --check")

    def verify_jar(self, root, cache_hit, content, curl_exit=0):
        jar = root / "openapi-generator-cli-test.jar"
        curl_log = root / "curl-args"
        curl = root / "curl"
        curl.write_text(
            '#!/bin/sh\nset -eu\n'
            'printf "%s\\n" "$@" > "$CURL_LOG"\n'
            'for output do :; done\n'
            'printf "%s" "$CURL_CONTENT" > "$output"\n'
            'exit "$CURL_EXIT"\n'
        )
        curl.chmod(0o700)
        env = {
            "PATH": str(root) + os.pathsep + os.defpath,
            "CACHE_HIT": cache_hit,
            "GENERATOR_URL": "https://unused.invalid/generator.jar",
            "GENERATOR_SHA256": hashlib.sha256(b"synthetic JAR").hexdigest(),
            "OPENAPI_GENERATOR_JAR": str(jar),
            "CURL_LOG": str(curl_log),
            "CURL_CONTENT": content,
            "CURL_EXIT": str(curl_exit),
        }
        if cache_hit == "true":
            jar.write_text(content)
        result = self.run_bash_step(self.verify, env)
        self.assertEqual(list(root.glob(jar.name + ".*")), [], "temporary download was left behind")
        return result, jar, curl_log

    def test_finance_generator_cache_hit_checks_digest_without_curl(self):
        for content, status in (("synthetic JAR", 0), ("corrupt", 1)):
            with self.subTest(content=content), tempfile.TemporaryDirectory(dir="/tmp") as directory:
                result, jar, curl_log = self.verify_jar(Path(directory), "true", content)
                self.assertEqual(result.returncode, status, result.stdout + result.stderr)
                self.assertFalse(curl_log.exists())
                self.assertEqual(jar.read_text(), content)

    def test_finance_generator_cache_miss_uses_bounded_retries_and_verifies_before_publishing(self):
        for cache_hit, content, curl_exit, expected_exit in (
            ("", "synthetic JAR", 0, 0),
            ("false", "corrupt", 0, 1),
            ("", "partial", 22, 22),
        ):
            with self.subTest(content=content), tempfile.TemporaryDirectory(dir="/tmp") as directory:
                result, jar, curl_log = self.verify_jar(Path(directory), cache_hit, content, curl_exit)
                self.assertEqual(result.returncode, expected_exit, result.stdout + result.stderr)
                args = curl_log.read_text().splitlines()
                self.assertEqual(args[:-1], [
                    "--fail", "--location", "--silent", "--show-error", "--retry", "3", "--retry-all-errors",
                    "--retry-max-time", "90", "--max-time", "30", "https://unused.invalid/generator.jar", "-o",
                ])
                self.assertTrue(args[-1].startswith(str(jar) + "."))
                if expected_exit:
                    self.assertFalse(jar.exists())
                else:
                    self.assertEqual(jar.read_text(), content)
