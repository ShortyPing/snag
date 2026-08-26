use std::process::Command;

// The commit hash is baked in at compile time so `snag revision` can report the
// exact source it was built from. CI passes SNAG_GIT_COMMIT explicitly; a local
// build falls back to asking git, and to "unknown" outside a checkout.
fn main() {
    println!("cargo:rerun-if-env-changed=SNAG_GIT_COMMIT");
    println!("cargo:rerun-if-changed=.git/HEAD");

    let commit = std::env::var("SNAG_GIT_COMMIT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(git_head)
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=SNAG_GIT_COMMIT={commit}");
}

fn git_head() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!sha.is_empty()).then_some(sha)
}
