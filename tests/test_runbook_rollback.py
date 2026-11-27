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

    def assert_rollback_order(self, *markers):
        offset = 0
        for marker in markers:
            position = self.rollback.find(marker, offset)
            self.assertNotEqual(position, -1, f"Missing or out of order: {marker}")
            offset = position + len(marker)

    def test_supervisor_exits_and_postgres_runs_alone_before_restore(self):
        self.assert_rollback_order(
            "`scripts/fertile down`",
            "确认 supervisord 已退出",
            "`scripts/postgres.sh start` 单独启动 PostgreSQL",
            'scripts/restore.sh "$pre_migration_snapshot" --dry-run',
            'scripts/restore.sh "$pre_migration_snapshot" \\\n',
            "`scripts/postgres.sh stop`",
            "scripts/fertile ensure-up",
        )

    def test_notifier_stays_disabled_until_queue_reconciliation(self):
        switch = '"${old_env[@]}" FERTILE_ENABLE_WEB_NOTIFIER=0 scripts/fertile rollback --switch-only "$old_sha"'
        disabled = '"${old_env[@]}" FERTILE_ENABLE_WEB_NOTIFIER=0 scripts/fertile ensure-up'
        enabled = '"${old_env[@]}" FERTILE_ENABLE_WEB_NOTIFIER=1 scripts/fertile ensure-up'
        self.assertEqual(self.rollback.count("scripts/fertile ensure-up"), 2)
        self.assertEqual(re.findall(r"`([^`\n]*scripts/fertile ensure-up)`", self.rollback), [disabled, enabled])
        self.assert_rollback_order(
            "记录 notifier 原启用状态",
            switch,
            disabled,
            "每次 `ensure-up` 都沿用同一组恢复目标及 notifier 禁用设置",
            "`scripts/fertile status --json`",
            "确认 notifier 未运行",
            "完成[队列对账](#告警与邮件进程)",
            "对账和验证通过后，若原部署已启用 notifier 且旧发布包含该进程",
            enabled,
            "核对 notifier 已连接恢复库",
        )

    def test_instance_root_and_port_are_required_before_shutdown(self):
        self.assert_rollback_order(
            "暂停接入或停止服务前",
            "核对目标实例的绝对持久根、PostgreSQL 端口和进程管理器",
            "export FERTILE_ROOT FERTILE_POSTGRES_PORT",
            "`sudo systemctl stop fertile.target`",
            "`scripts/fertile down`",
        )
        blocks = re.findall(r"^```bash\n(.*?)^```$", self.rollback, re.M | re.S)
        self.assertEqual(len(blocks), 1)
        script = (
            "FERTILE_ROOT=$1\nFERTILE_POSTGRES_PORT=$2\n"
            "check_instance() {\n" + blocks[0] + "}\n"
            "check_instance && /bin/sh -c 'printf \"%s\\n\" \"$FERTILE_ROOT\" \"$FERTILE_POSTGRES_PORT\"'"
        )
        instance = str(ROOT)
        for case, root, port, ready in (
            ("confirmed instance", instance, "55432", True),
            ("missing root", "", "55432", False),
            ("missing port", instance, "", False),
            ("relative root", ".", "55432", False),
            ("privileged port", instance, "1023", False),
            ("out-of-range port", instance, "65536", False),
        ):
            with self.subTest(case=case):
                result = subprocess.run(
                    ["/bin/bash", "--noprofile", "--norc", "-c", script, "check-instance", root, port],
                    env={}, capture_output=True, text=True, timeout=5,
                )
                if ready:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout.splitlines(), [root, port])
                else:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(result.stdout, "")

    def test_systemd_has_its_own_stop_restore_and_start_path(self):
        systemd_heading = "\n#### systemd 实例\n"
        supervisord_heading = "\n#### supervisord 实例\n"
        self.assert_rollback_order(systemd_heading, supervisord_heading)
        preparation, branches = self.rollback.split(systemd_heading, 1)
        systemd, supervisord = branches.split(supervisord_heading, 1)
        for marker in (
            "fertile.target` 的单元绑定到已确认的实例根",
            "ops/backup.md#恢复步骤",
            "`sudo systemctl stop fertile.target`",
            "`web_rollback_<sha>`",
            "`data-paper-rollback-<sha>`",
            "deploy/linux-server.md#systemd",
        ):
            self.assertIn(marker, systemd)
        self.assert_rollback_order(
            systemd_heading, "fertile.target` 的单元绑定到已确认的实例根",
            "`sudo systemctl stop fertile.target`", "单独启动该实例的 PostgreSQL",
            "完成预检与正式恢复", "停止单独启动的 PostgreSQL",
            "`FERTILE_ENABLE_WEB_NOTIFIER=0`", "再由 systemd 启动",
            "完成[队列对账](#告警与邮件进程)", "再按原启用状态恢复投递", supervisord_heading,
        )
        for command in ("scripts/fertile down", "scripts/fertile rollback", "scripts/fertile ensure-up"):
            self.assertNotIn(command, preparation + systemd)
            self.assertIn(command, supervisord)
