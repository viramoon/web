//! Owns an isolated PostgreSQL process and removes its temporary cluster on drop.
use std::process::Command;
use tempfile::TempDir;

pub(crate) fn checked(command: &mut Command) -> String {
    let output = command.output().unwrap_or_else(|error| {
        panic!("could not execute {:?}: {error}", command.get_program());
    });
    assert!(
        output.status.success(),
        "{command:?}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn local_command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
}

pub(crate) struct Cluster {
    dir: TempDir,
}

impl Cluster {
    pub(crate) fn create() -> Self {
        let dir = tempfile::tempdir().unwrap();
        checked(
            local_command("initdb")
                .args([
                    "--username=web_test",
                    "--auth-local=trust",
                    "--auth-host=reject",
                    "--encoding=UTF8",
                    "--no-locale",
                    "-D",
                ])
                .arg(dir.path().join("data")),
        );
        let pg = Self { dir };
        pg.start();
        checked(pg.client("createdb").arg("web_test_db"));
        pg
    }
    fn ctl(&self) -> Command {
        let mut c = local_command("pg_ctl");
        c.arg("-D")
            .arg(self.dir.path().join("data"))
            .args(["-w", "-t", "15"]);
        c
    }
    pub(crate) fn start(&self) {
        checked(
            self.ctl()
                .arg("-l")
                .arg(self.dir.path().join("postgres.log"))
                .args(["start", "-o"])
                .arg(format!("-h '' -k '{}' -p 55453", self.dir.path().display())),
        );
    }
    pub(crate) fn stop(&self) {
        checked(self.ctl().args(["-m", "fast", "stop"]));
    }
    pub(crate) fn url(&self) -> String {
        format!(
            "postgresql://web_test@localhost/web_test_db?host={}&port=55453&password=&sslmode=disable",
            self.dir.path().display()
        )
    }
    pub(crate) fn client(&self, bin: &str) -> Command {
        let mut command = local_command(bin);
        command
            .env("PGHOST", self.dir.path())
            .env("PGPORT", "55453")
            .env("PGPASSFILE", self.dir.path().join("absent-pgpass"))
            .env("PGSSLMODE", "disable")
            .env("PGUSER", "web_test");
        command
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        let _ = self.ctl().args(["-m", "immediate", "stop"]).output();
    }
}
