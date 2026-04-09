#[test]
fn integration_links_against_lib() {
    assert_eq!(nodeps_lib::greet(), "Hello from cargo-nix-plugin!");
}
