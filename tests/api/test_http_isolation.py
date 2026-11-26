"""Exercise the direct-run test entry points without opening sockets."""

from email.message import Message
import io
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit
from urllib.request import HTTPHandler
from urllib.response import addinfourl

import core_http
import finance_resilience
import finance_writes
import http_fixture


FAKE_TOKEN = "foreign-synthetic-token"
FOREIGN_ENV = {
    "http_proxy": "http://proxy.invalid:4321",
    "HTTPS_PROXY": "http://proxy.invalid:4321",
    "ALL_PROXY": "http://proxy.invalid:4321",
    "DATABASE_URL": "postgresql://db.invalid/elsewhere",
    "FERTILE_PROFILE": "stable",
    "WEB_AUTO_MIGRATE": "true",
    "WEB_CONFIG_DIR": "/tmp/foreign-config",
    "WEB_STATIC_ROOT": "/tmp/foreign-dist",
    "WEB_ADMIN_TOKEN_FILE": "/tmp/foreign-admin.env",
    "WEB_ADMIN_TOKEN": FAKE_TOKEN,
    "WEB_CREDENTIAL_DIR": "/tmp/foreign-storage",
    "WEB_CREDENTIAL_RECIPIENT_PUBLIC_KEY_FILE": "/tmp/foreign-recipient.pem",
    "WEB_PUBLIC_ORIGIN": "https://foreign.invalid",
    "WEB_PORT": "9123",
    "WEB_BIND_HOST": "0.0.0.0",
    "FINANCE_BASE_URL": "https://finance.invalid",
    "PGSERVICE": "foreign",
    "PGPASSFILE": "/tmp/foreign-pgpass",
    "WEB_BIN": "/tmp/web-test-executable",
}


class StopBeforeSpawn(Exception):
    pass


class HttpIsolationTests(unittest.TestCase):
    def test_all_python_entrypoints_spawn_with_only_fixture_settings(self):
        mock = SimpleNamespace(server_port=62001, env_decoy="local-synthetic-token")
        for module in (core_http, finance_resilience, finance_writes):
            with self.subTest(module=module.__name__), tempfile.TemporaryDirectory(dir="/tmp") as directory:
                root = Path(directory)
                token_file = root / "test.env"
                socket_dir = root / "pg"
                with (
                    patch.dict(os.environ, FOREIGN_ENV, clear=True),
                    patch.object(module, "reserve_port", return_value=62002),
                    patch.object(module.subprocess, "Popen", side_effect=StopBeforeSpawn) as spawn,
                    patch.object(module.subprocess, "run") as command,
                    self.assertRaises(StopBeforeSpawn),
                ):
                    if module is core_http:
                        module.run(root, mock, socket_dir)
                    elif module is finance_resilience:
                        module.run(root, mock)
                    else:
                        with module.web(root, mock, token_file, 10):
                            self.fail("unexpected service start")
                self.assertEqual(spawn.call_args.args[0], [FOREIGN_ENV["WEB_BIN"], "serve"])
                env = spawn.call_args.kwargs["env"]
                expected = {
                    "FERTILE_PROFILE": "cloudbox", "WEB_CONFIG_DIR": str(root / "config"),
                    "WEB_BIND_HOST": "127.0.0.1", "WEB_PORT": "62002",
                    "WEB_STATIC_ROOT": str(root if module is finance_writes else root / "dist"),
                    "WEB_ADMIN_TOKEN_FILE": str(token_file if module is finance_writes
                                                else root / "absent-admin.env"),
                    "WEB_AUTO_MIGRATE": "false", "FINANCE_BASE_URL": "http://127.0.0.1:62001",
                    "NO_PROXY": "*", "no_proxy": "*", "RUST_LOG": "info",
                }
                url = env["DATABASE_URL"]
                self.assertEqual(urlsplit(url).hostname, "localhost")
                self.assertEqual(parse_qs(urlsplit(url).query, keep_blank_values=True), {
                    "host": [str(socket_dir if module is core_http else root / "no-db")],
                    "port": ["5432"],
                    "password": [""], "sslmode": ["disable"],
                })
                expected["DATABASE_URL"] = url
                if module is finance_writes:
                    expected["WEB_ADMIN_TOKEN"] = mock.env_decoy
                self.assertEqual(env, expected)
                if module is core_http:
                    command.assert_called_once_with([FOREIGN_ENV["WEB_BIN"], "migrate"], env=env, check=True)
                else:
                    command.assert_not_called()

    def test_http_clients_neither_proxy_nor_follow_redirects(self):
        for request in (core_http.get, finance_resilience.get, finance_writes.request):
            with self.subTest(request=request.__module__):
                seen = []

                def fake_http_open(_, req):
                    seen.append((req.host, req.selector))
                    headers = Message()
                    headers["Location"] = "http://redirect.invalid/synthetic"
                    response = addinfourl(io.BytesIO(b"{}"), headers, req.full_url, 302)
                    response.msg = "Found"
                    return response

                with (
                    patch.dict(os.environ, FOREIGN_ENV, clear=True),
                    patch("urllib.request._opener", None),
                    patch.object(HTTPHandler, "http_open", fake_http_open),
                ):
                    self.assertEqual(request(62002, "/probe")[0], 302)
                self.assertEqual(seen, [("127.0.0.1:62002", "/probe")])

    def test_postgres_fixture_uses_a_private_socket_and_stops_on_exit(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            root = Path(directory)
            with (
                patch.dict(os.environ, FOREIGN_ENV, clear=True),
                patch.object(http_fixture.shutil, "which", side_effect=lambda name: "/tmp/tool-" + name),
                patch.object(http_fixture.subprocess, "run") as run,
            ):
                with http_fixture.postgres(root) as socket_dir:
                    self.assertEqual(socket_dir, root / "pg")
                    (root / "pgdata").mkdir()
                    (root / "pgdata" / "postmaster.pid").touch()
                commands = [record.args[0] for record in run.call_args_list]
                self.assertIn("--auth-host=reject", commands[0])
                self.assertEqual(commands[1][-1], f"-h '' -k {socket_dir} -p 5432")
                self.assertEqual(commands[2][-3:], ["-m", "immediate", "stop"])
                for record in run.call_args_list:
                    self.assertEqual(record.kwargs["env"], {"LC_ALL": "C", "PATH": os.defpath})
