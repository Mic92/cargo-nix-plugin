// Regression: cargo's autobins also discovers `src/bin/<name>/main.rs`.
// In lockfile-resolve mode the eval-time `crateBin` is empty for workspace
// members that rely purely on layout convention, so this exercises the
// `resolve_bins` autodiscovery branch.
mod helper;

fn main() {
    println!("multifile {}", helper::msg());
}
