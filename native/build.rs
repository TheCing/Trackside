//! Build script. Sole job: make cargo re-evaluate the crate when `TRACKSIDE_DEV` changes so
//! the self-updater's `option_env!("TRACKSIDE_DEV")` dev-build guard can't get stuck as a
//! stale cached value when switching between dev builds (Build-Trackside.ps1 sets it) and
//! release builds (the release tool doesn't).
fn main() {
    println!("cargo:rerun-if-env-changed=TRACKSIDE_DEV");
}
