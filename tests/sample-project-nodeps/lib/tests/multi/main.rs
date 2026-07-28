// Multi-file integration test target (tests/<name>/main.rs shape).
mod helper;

#[test]
fn multi_file_test_uses_helper() {
    assert_eq!(helper::double(21), 42);
}
