//! In-overlay update helper for dev/collaborator builds (those who clone the
//! repo). Checks the upstream git remote for new commits and can `git pull`.
//! End users on a zip have no `.git` — the "Releases" link covers them. This is
//! a convenience, NOT an advantage feature, so it ships in every build.
//!
//! A loaded native DLL can't hot-swap itself, so `pull()` only fetches the new
//! source; the user still rebuilds (`cargo build --release …`) and relaunches.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const NO_WINDOW: u32 = 0x0800_0000; // CREATE_NO_WINDOW — no console flash

pub const RELEASES_URL: &str =
    "https://github.com/Nighty3333/Heaven-Internal-Public-Version-/releases";

static STATUS: OnceLock<Mutex<String>> = OnceLock::new();
fn status_slot() -> &'static Mutex<String> {
    STATUS.get_or_init(|| Mutex::new(String::new()))
}
static BUSY: AtomicBool = AtomicBool::new(false);

pub fn status() -> String {
    status_slot().lock().map(|s| s.clone()).unwrap_or_default()
}
pub fn is_busy() -> bool {
    BUSY.load(Ordering::Relaxed)
}
fn set_status(s: impl Into<String>) {
    if let Ok(mut g) = status_slot().lock() {
        *g = s.into();
    }
}


fn repo_root() -> PathBuf {
    PathBuf::from(".")
}

fn has_repo(root: &Path) -> bool {
    root.join(".git").exists()
}

fn git(root: &Path, args: &[&str]) -> Option<std::process::Output> {
    let mut c = Command::new("git");
    c.arg("-C").arg(root);
    for a in args {
        c.arg(a);
    }
    #[cfg(windows)]
    c.creation_flags(NO_WINDOW);
    c.output().ok()
}

/// Fetch upstream and report how many commits we're behind. Background thread so
/// the render loop never blocks on the network.
pub fn check() {
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(|| {
        let root = repo_root();
        if !has_repo(&root) {
            set_status("No local repo \u{2014} use Releases \u{2197}");
            BUSY.store(false, Ordering::SeqCst);
            return;
        }
        set_status("Checking\u{2026}");
        let _ = git(&root, &["fetch", "--quiet"]);
        let behind = git(&root, &["rev-list", "--count", "HEAD..@{u}"])
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().parse::<u32>().unwrap_or(0))
            .unwrap_or(0);
        if behind == 0 {
            set_status("Up to date \u{2713}");
        } else {
            set_status(format!("{behind} update(s) \u{2014} git pull below"));
        }
        BUSY.store(false, Ordering::SeqCst);
    });
}

/// `git pull --ff-only` the latest source. Background thread. The user still
/// rebuilds + relaunches (a loaded DLL can't replace itself live).
pub fn pull() {
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(|| {
        let root = repo_root();
        if !has_repo(&root) {
            set_status("No local repo \u{2014} use Releases \u{2197}");
            BUSY.store(false, Ordering::SeqCst);
            return;
        }
        set_status("Pulling\u{2026}");
        match git(&root, &["pull", "--ff-only"]) {
            Some(o) if o.status.success() => {
                let out = String::from_utf8_lossy(&o.stdout);
                if out.contains("Already up to date") {
                    set_status("Already up to date \u{2713}");
                } else {
                    set_status("Pulled \u{2014} rebuild & restart to apply");
                }
            }
            Some(o) => {
                let e = String::from_utf8_lossy(&o.stderr);
                set_status(format!("Pull failed: {}", e.lines().next().unwrap_or("error")));
            }
            None => set_status("git not found in PATH"),
        }
        BUSY.store(false, Ordering::SeqCst);
    });
}
