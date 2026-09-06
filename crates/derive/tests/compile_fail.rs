#[test]
fn invalid_error_declarations_explain_the_missing_contract() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/error_*.rs");
}
