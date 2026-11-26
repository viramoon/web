"""Local HTTP and subprocess settings shared by the executable smoke tests."""

from contextlib import contextmanager
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
from urllib.parse import urlencode
from urllib.request import HTTPRedirectHandler, ProxyHandler, build_opener


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


local_http = build_opener(ProxyHandler({}), NoRedirect())


def web_env(root, port, finance_port, *, static=None, socket_dir=None, token_file=None, profile=None):
    config = root / "config"
    config.mkdir(exist_ok=True)
    base_yaml = Path(__file__).resolve().parents[2] / "config/base.yaml"
    shutil.copyfile(base_yaml, config / "base.yaml")
    (config / "cloudbox.yaml").write_text(json.dumps(profile or {}))
    # An explicit empty password keeps SQLx from looking for a pgpass file.
    socket_query = urlencode({
        "host": socket_dir or root / "no-db", "port": "5432", "password": "", "sslmode": "disable",
    })
    return {
        "FERTILE_PROFILE": "cloudbox",
        "WEB_CONFIG_DIR": str(config),
        "WEB_BIND_HOST": "127.0.0.1",
        "WEB_PORT": str(port),
        "WEB_STATIC_ROOT": str(static or root),
        "WEB_ADMIN_TOKEN_FILE": str(token_file or root / "absent-admin.env"),
        "WEB_AUTO_MIGRATE": "false",
        "DATABASE_URL": f"postgresql://web_test@localhost/postgres?{socket_query}",
        "FINANCE_BASE_URL": f"http://127.0.0.1:{finance_port}",
        "NO_PROXY": "*",
        "no_proxy": "*",
        "RUST_LOG": "info",
    }


@contextmanager
def postgres(root):
    data = root / "pgdata"
    socket_dir = root / "pg"
    socket_dir.mkdir(mode=0o700)
    tools = {name: shutil.which(name) for name in ("initdb", "pg_ctl")}
    if not all(tools.values()):
        raise RuntimeError("Add the PostgreSQL bin directory containing initdb and pg_ctl to PATH")
    env = {"LC_ALL": "C", "PATH": os.defpath}
    subprocess.run([
        tools["initdb"], "--username=web_test", "--auth-local=trust", "--auth-host=reject",
        "--encoding=UTF8", "--no-locale", "-D", str(data),
    ], env=env, stdout=subprocess.DEVNULL, check=True)
    ctl = [tools["pg_ctl"], "-D", str(data), "-w", "-t", "15"]
    started = False
    try:
        subprocess.run([
            *ctl, "-l", str(root / "postgres.log"), "start",
            "-o", f"-h '' -k {shlex.quote(str(socket_dir))} -p 5432",
        ], env=env, stdout=subprocess.DEVNULL, check=True)
        started = True
        yield socket_dir
    finally:
        if (data / "postmaster.pid").exists():
            subprocess.run([*ctl, "-m", "immediate", "stop"], env=env,
                           stdout=subprocess.DEVNULL, check=started)
