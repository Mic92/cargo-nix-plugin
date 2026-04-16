// Inferred bin: must still be built even though Cargo.toml has an
// explicit [[bin]] entry for `explicit`.
//
// `async fn` is a hard error on edition 2015, so this also proves
// edition.workspace=true was inherited from [workspace.package].
async fn _edition_probe() {}

fn main() {
    println!("mixed-main ok");
}
