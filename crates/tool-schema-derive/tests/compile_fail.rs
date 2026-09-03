//! Compile-fail coverage for `ToolSchema`'s derive diagnostics.
//!
//! Each case in `tests/ui/` pairs a rejected input with a checked `.stderr`
//! golden, proving the derive's diagnostics stay spanned on the offending
//! field rather than the macro call site.

#[test]
fn missing_description_names_its_field() {
    trybuild::TestCases::new().compile_fail("tests/ui/missing_description.rs");
}

#[test]
fn contradictory_name_names_its_field() {
    trybuild::TestCases::new().compile_fail("tests/ui/contradictory_name.rs");
}

#[test]
fn unsupported_type_names_its_field() {
    trybuild::TestCases::new().compile_fail("tests/ui/unsupported_type.rs");
}

#[test]
fn missing_schema_impl_names_its_field() {
    trybuild::TestCases::new().compile_fail("tests/ui/missing_schema_impl.rs");
}

#[test]
fn flatten_names_its_field() {
    trybuild::TestCases::new().compile_fail("tests/ui/flatten.rs");
}

#[test]
fn custom_decoder_without_shape_names_its_field() {
    trybuild::TestCases::new().compile_fail("tests/ui/custom_decoder_without_shape.rs");
}

#[test]
fn wire_shape_without_custom_decoder_names_its_field() {
    trybuild::TestCases::new().compile_fail("tests/ui/wire_shape_without_custom_decoder.rs");
}

#[test]
fn duplicate_attribute_spans_the_repeated_key() {
    trybuild::TestCases::new().compile_fail("tests/ui/duplicate_attribute.rs");
}

#[test]
fn unsupported_rename_rule_spans_the_offending_literal() {
    trybuild::TestCases::new().compile_fail("tests/ui/unsupported_rename_rule.rs");
}
