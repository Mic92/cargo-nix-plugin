// Regression: env!() in build.rs is resolved at *compile* time, so the
// build-script compile rustc invocation must already have CARGO_PKG_* /
// CARGO_MANIFEST_DIR in its environment, not just the run.
const _NAME: &str = env!("CARGO_PKG_NAME");
const _VER: &str = env!("CARGO_PKG_VERSION");
const _MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

fn main() {
    // Exercise the run-time env path too: a directive consumed by the lib
    // build, and a value derived from a CARGO_* var.
    println!("cargo:rustc-cfg=nodeps_build_ok");
    println!(
        "cargo:rustc-env=NODEPS_BUILD_PKG={}",
        std::env::var("CARGO_PKG_NAME").unwrap()
    );
}
