#[test]
fn invalid_error_declarations_explain_the_missing_contract() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/error_*.rs");
}

#[test]
fn valid_error_fields_compile() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui/pass_error_*.rs");
}

#[test]
fn invalid_accessor_declarations_explain_the_field_contract() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/accessor_*.rs");
}
