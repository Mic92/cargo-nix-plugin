#[test]
fn integration_links_against_lib() {
    assert_eq!(nodeps_lib::greet(), "Hello from cargo-nix-plugin!");
}

// This test fails only under NEXTEST_TEST_MUST_FAIL=1. It lets
// nextest-run-test assert that a failing test fails the derivation.
#[test]
fn env_controlled_failure() {
    assert!(std::env::var("NEXTEST_TEST_MUST_FAIL").is_err());
}
