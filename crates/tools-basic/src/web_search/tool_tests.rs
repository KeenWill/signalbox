use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};
use signalbox_domain::{ToolEffectClass, ToolPermissionDefault};

use super::{test_provider_support::*, test_service_support::*, test_support::*, tool::*};

/// The provider read is auto-approved but remains crash-relevant because
/// the remote provider observes the authenticated GET.
#[test]
fn web_search_definition_carries_exact_policy() {
    let (catalog, _executor) = WebSearchTool::try_new((), (), configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let definitions = catalog.definitions();
    assert_eq!(definitions.len(), 1);
    let definition = &definitions[0];

    assert_eq!(definition.name().as_str(), WEB_SEARCH_NAME);
    assert_eq!(definition.permission_default(), ToolPermissionDefault::Auto);
    assert_eq!(definition.effect_class(), ToolEffectClass::ExternalEffect);
}

/// The rendered schema is the exact query-only wire artifact.
#[test]
fn web_search_rendered_schema_is_the_exact_wire_artifact() {
    let (catalog, _executor) = WebSearchTool::try_new((), (), configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let definition = &catalog.definitions()[0];
    let schema: serde_json::Value = serde_json::from_str(definition.input_schema().as_str())
        .expect("registry schema is valid JSON");

    expect_test::expect![[r#"
        {
          "additionalProperties": false,
          "properties": {
            "query": {
              "description": "Nonempty query of at most 400 characters and 50 words.",
              "maxLength": 400,
              "minLength": 1,
              "type": "string"
            }
          },
          "required": [
            "query"
          ],
          "type": "object"
        }"#]]
    .assert_eq(&format!("{schema:#}"));
    assert_eq!(definition.input_schema().as_str(), schema.to_string());
}

/// Typed decoding accepts the documented bounded query shape.
#[test]
fn web_search_typed_decode_accepts_bounded_query() {
    let (catalog, _executor) = WebSearchTool::try_new((), (), configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let definition = &catalog.definitions()[0];
    let supplied = serde_json::json!({"query": FIXTURE_QUERY}).to_string();

    assert_eq!(
        catalog.validate_arguments(definition.name(), &arguments(&supplied)),
        Ok(())
    );
}

/// Typed decoding rejects a query with no non-whitespace content.
#[test]
fn web_search_typed_decode_rejects_blank_query() {
    let (catalog, _executor) = WebSearchTool::try_new((), (), configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let definition = &catalog.definitions()[0];

    assert!(matches!(
        catalog.validate_arguments(definition.name(), &arguments(r#"{"query":"   "}"#)),
        Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
    ));
}
