use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-changed=migrations");
    let mut refs = vec!["HEAD".to_owned(), "packed-refs".to_owned()];
    if let Some(branch) = git(&["symbolic-ref", "--quiet", "HEAD"]) {
        refs.push(branch);
    }
    for reference in refs {
        if let Some(path) = git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            &reference,
        ]) {
            // Works for ordinary checkouts, linked worktrees and submodule gitfiles.
            if reference != "packed-refs" || std::path::Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    let sha = git(&["rev-parse", "--verify", "HEAD"])
        .or_else(|| std::env::var("GITHUB_SHA").ok())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=WEB_GIT_SHA={sha}");
}
