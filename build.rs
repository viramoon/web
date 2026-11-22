use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Ok(output) = Command::new("git")
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .output()
        && output.status.success()
    {
        println!(
            "cargo:rerun-if-changed=.git/{}",
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }
    let sha = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .or_else(|| std::env::var("GITHUB_SHA").ok())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=WEB_GIT_SHA={sha}");
}
