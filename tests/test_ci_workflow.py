"""Offline structure checks for the workflows that run web verification."""

import os
from pathlib import Path
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
