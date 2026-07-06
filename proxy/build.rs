//! Feed the export table to the linker from `version.def`. Every version.dll API
//! (plus UnityMain/UnityMain2) is a static forwarder there, so the loader resolves
//! them to the genuine DLL without running any of our code — the fix for the
//! GameAssembly.dll boot fault the runtime-forwarding version caused. See
//! `version.def` and `CRASH-NOTES.md`.
fn main() {
    let def = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("version.def");
    // cdylib-specific: applies the /DEF only to the version.dll artifact.
    println!("cargo:rustc-cdylib-link-arg=/DEF:{}", def.display());
    println!("cargo:rerun-if-changed=version.def");
}
