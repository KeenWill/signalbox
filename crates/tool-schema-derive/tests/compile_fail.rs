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
