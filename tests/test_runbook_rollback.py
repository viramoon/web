"""Check the migration rollback policy and run its example with a local restore stub."""

from pathlib import Path
import re
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
HEADING = "### 含新增迁移的发布回退"
ANCHOR = "#含新增迁移的发布回退"
OLD_SHA = "abcdef0"


class RunbookRollbackTests(unittest.TestCase):
    def setUp(self):
        self.runbook = (ROOT / "RUNBOOK.md").read_text(encoding="utf-8")
        self.rollback = self.runbook.split(HEADING + "\n", 1)[1].split("\n## ", 1)[0]

    def test_release_rollback_requires_the_pre_migration_snapshot(self):
        for policy in (
            "旧代码绝不在新发布写过的数据库上运行",
            "从同一迁移前快照恢复",
            "web_rollback_<sha>",
            "data-paper-rollback-<sha>",
            "然后在这套目标上运行旧发布",
            "将 `web` 和 `web-notifier` 的 `DATABASE_URL` 指向同一个恢复库",
        ):
            self.assertIn(policy, self.rollback)
        self.assertNotIn("WEB_AUTO_MIGRATE", self.rollback)

        for name in ("RUNBOOK.md", "docs/database.md"):
            with self.subTest(name=name):
                document = (ROOT / name).read_text(encoding="utf-8")
                self.assertNotRegex(document, r"保留(?:前向|数据库及已执行迁移)|确认数据库向后兼容")
                for line in document.splitlines():
                    if "WEB_AUTO_MIGRATE=false" in line:
                        self.assertNotRegex(line, r"回退|回滚|旧版|旧 web")

    def test_related_documents_link_to_the_rollback_procedure(self):
        for name in ("README.md", "docs/database.md", "docs/api.md", "migrations/README.md", "service.yaml"):
            with self.subTest(name=name):
                document = (ROOT / name).read_text(encoding="utf-8")
                self.assertIn("RUNBOOK.md" + ANCHOR, document)

    def run_example(self, snapshot_has_dump=True, sha=OLD_SHA, dry_run_status="0"):
        blocks = re.findall(r"^   ```bash\n(.*?)^   ```$", self.rollback, re.M | re.S)
        self.assertEqual(len(blocks), 1)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            snapshot = root / "synthetic snapshot"
            snapshot.mkdir()
            if snapshot_has_dump:
                (snapshot / "postgres.dump").touch()
            instance = root / "isolated instance"
            instance.mkdir()
            log = root / "restore-arguments"
            scripts = root / "scripts"
            scripts.mkdir()
            restore = scripts / "restore.sh"
            restore.write_text(
                '#!/bin/sh\n'
                'printf "%s\\n" "$@" >> "$RESTORE_LOG"\n'
                'if [ "$2" = --dry-run ]; then exit "$DRY_RUN_STATUS"; fi\n',
                encoding="utf-8",
            )
            restore.chmod(0o700)
            result = subprocess.run(
                ["/bin/bash", "--noprofile", "--norc", "-c", blocks[0]],
                cwd=root,
                env={
                    "PATH": str(root / "no-tools"),
                    "FERTILE_ROOT": str(instance),
                    "old_sha": sha,
                    "pre_migration_snapshot": str(snapshot),
                    "RESTORE_LOG": str(log),
                    "DRY_RUN_STATUS": dry_run_status,
                },
                capture_output=True,
                text=True,
                timeout=5,
            )
            calls = log.read_text(encoding="utf-8").splitlines() if log.exists() else []
            target = [
                "--target-data-root", str(instance / f"data-paper-rollback-{OLD_SHA}"),
                "--target-database", f"web_rollback_{OLD_SHA}",
            ]
            return result, calls, [str(snapshot), "--dry-run", *target], [str(snapshot), *target]

    def test_restore_example_targets_only_the_isolated_database_and_root(self):
        result, calls, preview, restore = self.run_example()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, preview + restore)

    def test_restore_example_stops_when_preflight_fails(self):
        result, calls, preview, _ = self.run_example(dry_run_status="7")
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(calls, preview)

    def test_restore_example_rejects_invalid_sha_and_missing_dump(self):
        for options in ({"sha": "../web"}, {"snapshot_has_dump": False}):
            with self.subTest(options=options):
                result, calls, _, _ = self.run_example(**options)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, [])
