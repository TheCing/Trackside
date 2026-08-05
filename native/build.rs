//! Build script. Two jobs:
//!
//! 1. Make cargo re-evaluate the crate when `TRACKSIDE_DEV` changes so the self-updater's
//!    `option_env!("TRACKSIDE_DEV")` dev-build guard can't get stuck as a stale cached value when
//!    switching between dev builds (Build-Trackside.ps1 sets it) and release builds (the release
//!    tool doesn't).
//!
//! 2. Stamp the short git commit into the binary as `TRACKSIDE_BUILD`, so the boot banner can say
//!    WHICH build a log came from. Same-tag hotfixes reuse the version string - 1.0.7 shipped two
//!    of them - so "Trackside v1.0.8" alone cannot tell an original from a hotfix, and the archived
//!    .pdb for one will not symbolize a stack from the other. Without this stamp the per-release
//!    symbols are only half useful.
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=TRACKSIDE_DEV");

    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    // Mark a dirty tree so a hand-built DLL is never mistaken for the tagged commit it sits on.
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let stamp = if dirty { format!("{commit}+") } else { commit };
    println!("cargo:rustc-env=TRACKSIDE_BUILD={stamp}");

    // HEAD moves without any tracked source changing (a new commit, a checkout), and the stamp has
    // to follow it - otherwise a rebuild keeps baking in a stale commit id.
    for p in [".git/HEAD", "../.git/HEAD"] {
        if std::path::Path::new(p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }
}
