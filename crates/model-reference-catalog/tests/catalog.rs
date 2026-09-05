use std::{collections::BTreeSet, process::Command};

use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_model_reference_catalog::{
    ActualBillingKind, BUNDLED_CATALOG_JSON, Catalog, CommercialChannel, MappingQuality,
    PriceResolution, Provider, RateDimension, ReferenceResolution, bundled_catalog,
    render_projections,
};

const REFERENCE_PACKAGE: &str = "signalbox-model-reference-catalog";

fn resolved_family_id(resolution: &ReferenceResolution) -> Option<&str> {
    match resolution {
        ReferenceResolution::FamilyOnly { family_id, .. } => Some(family_id),
        ReferenceResolution::Resolved { .. }
        | ReferenceResolution::Ambiguous { .. }
        | ReferenceResolution::Unknown => None,
    }
}

fn api_model_rate_set_ids(catalog: &Catalog, model_hint: &str, date: &str) -> Option<Vec<String>> {
    let resolution = catalog
        .resolve(Provider::Openai, model_hint, date, CommercialChannel::Api)
        .ok()?;
    Some(
        resolution
            .price()?
            .resolved_rate_sets()?
            .iter()
            .map(|rate_set| rate_set.id.clone())
            .collect(),
    )
}

fn consumer_mapping_mut<'a>(
    catalog: &'a mut Value,
    id: &str,
) -> Result<&'a mut Value, &'static str> {
    catalog["consumer_mappings"]
        .as_array_mut()
        .ok_or("consumer_mappings is not an array")?
        .iter_mut()
        .find(|mapping| mapping["id"] == id)
        .ok_or("consumer mapping fixture is absent")
}

#[test]
fn actual_billing_kind_is_distinct_from_equivalent_api_pricing() {
    assert_eq!(
        CommercialChannel::Api.actual_billing_kind(),
        ActualBillingKind::ApiMetered
    );
    assert_eq!(
        CommercialChannel::ClaudeCodeSubscription.actual_billing_kind(),
        ActualBillingKind::Subscription
    );
}

#[test]
fn exact_dated_claude_code_identity_resolves_october_2025_rate() {
    let catalog = bundled_catalog().unwrap();

    let resolution = catalog
        .resolve(
            Provider::Anthropic,
            "claude-sonnet-4-5-20250929",
            "2025-10-15",
            CommercialChannel::ClaudeCodeSubscription,
        )
        .unwrap();
    let rate_set = &resolution.price().unwrap().resolved_rate_sets().unwrap()[0];

    assert_eq!(
        resolution.resolved_model_id(),
        Some("anthropic:claude-sonnet-4-5-20250929")
    );
    assert_eq!(
        resolution.resolved_mapping_quality(),
        Some(MappingQuality::Exact)
    );
    assert_eq!(
        rate_set
            .rate(RateDimension::Input, "tier=standard, region=global")
            .unwrap()
            .usd_per_million_tokens,
        Some(Decimal::new(3, 0))
    );
    assert_eq!(
        rate_set
            .rate(RateDimension::Output, "tier=standard, region=global")
            .unwrap()
            .usd_per_million_tokens,
        Some(Decimal::new(15, 0))
    );
    assert_eq!(
        rate_set.source_ids,
        vec![String::from("anth-claude45-sonnet-2025-09-29")]
    );
}

#[test]
fn rolling_consumer_label_stops_at_model_family() {
    let catalog = bundled_catalog().unwrap();

    let resolution = catalog
        .resolve(
            Provider::Openai,
            "GPT-4o",
            "2024-11-01",
            CommercialChannel::ChatgptSubscription,
        )
        .unwrap();

    assert_eq!(
        resolution,
        ReferenceResolution::FamilyOnly {
            family_id: String::from("openai:gpt-4o-family"),
            mapping_confidence: signalbox_model_reference_catalog::Confidence::High,
            mapping_source_ids: vec![
                String::from("oai-gpt4o-api-2024-05-13"),
                String::from("oai-gpt4o-mini-2024-07-18"),
            ],
            limitations: vec![String::from(
                "A consumer label does not prove which dated API snapshot or internal product variant served a turn."
            )],
        }
    );
}

#[test]
fn rolling_api_alias_uses_its_explicit_pricing_reference() {
    let catalog = bundled_catalog().unwrap();

    let resolution = catalog
        .resolve(
            Provider::Openai,
            "gpt-5-chat-latest",
            "2025-08-08",
            CommercialChannel::Api,
        )
        .unwrap();
    let rate_set = &resolution.price().unwrap().resolved_rate_sets().unwrap()[0];

    assert_eq!(
        resolution.resolved_model_id(),
        Some("openai:gpt-5-chat-latest")
    );
    assert_eq!(rate_set.id, "oai-gpt5-standard");
}

#[test]
fn exact_api_snapshots_keep_their_recorded_rate_sets() {
    let catalog = bundled_catalog().unwrap();

    assert_eq!(
        api_model_rate_set_ids(&catalog, "gpt-4-0314", "2023-03-14"),
        Some(vec![String::from("oai-gpt4-launch")])
    );
    assert_eq!(
        api_model_rate_set_ids(&catalog, "gpt-4-32k-0314", "2023-03-14"),
        Some(vec![String::from("oai-gpt4-32k-launch")])
    );
    assert_eq!(
        api_model_rate_set_ids(&catalog, "gpt-4-1106-preview", "2023-11-06"),
        Some(vec![String::from("oai-gpt4-turbo-launch")])
    );
    assert_eq!(
        api_model_rate_set_ids(&catalog, "gpt-4o-2024-08-06", "2024-10-01"),
        Some(vec![String::from("oai-gpt4o-0806-caching")])
    );
}

#[test]
fn rolling_gpt4_alias_shares_its_launch_rate() {
    let catalog = bundled_catalog().unwrap();

    let resolution = catalog
        .resolve(
            Provider::Openai,
            "gpt-4",
            "2023-03-14",
            CommercialChannel::Api,
        )
        .unwrap();

    assert_eq!(resolution.resolved_model_id(), Some("openai:gpt-4"));
    assert_eq!(
        resolution.price().unwrap().resolved_rate_sets().unwrap()[0].id,
        "oai-gpt4-launch"
    );
}

#[test]
fn rolling_gpt4_32k_alias_shares_its_launch_rate() {
    let catalog = bundled_catalog().unwrap();

    let resolution = catalog
        .resolve(
            Provider::Openai,
            "gpt-4-32k",
            "2023-03-14",
            CommercialChannel::Api,
        )
        .unwrap();

    assert_eq!(resolution.resolved_model_id(), Some("openai:gpt-4-32k"));
    assert_eq!(
        resolution.price().unwrap().resolved_rate_sets().unwrap()[0].id,
        "oai-gpt4-32k-launch"
    );
}

#[test]
fn retired_snapshot_rate_window_ends_with_model_availability() {
    let catalog = bundled_catalog().unwrap();

    let resolution = catalog
        .resolve(
            Provider::Openai,
            "gpt-4-0314",
            "2024-06-13",
            CommercialChannel::Api,
        )
        .unwrap();
    let rate_set = &resolution.price().unwrap().resolved_rate_sets().unwrap()[0];

    assert_eq!(rate_set.interval, "2023-03-14..2024-06-14");
}

#[test]
fn codex_subscription_model_resolves_only_to_approximate_api_analogue() {
    let catalog = bundled_catalog().unwrap();

    let resolution = catalog
        .resolve(
            Provider::Openai,
            "codex-1",
            "2025-05-20",
            CommercialChannel::CodexSubscription,
        )
        .unwrap();

    assert_eq!(resolution.resolved_model_id(), Some("openai:o3"));
    assert_eq!(
        resolution.resolved_mapping_quality(),
        Some(MappingQuality::Approximate)
    );
}

#[test]
fn historical_o3_price_changes_at_published_transition() {
    let catalog = bundled_catalog().unwrap();

    let before = catalog
        .resolve(Provider::Openai, "o3", "2025-06-09", CommercialChannel::Api)
        .unwrap();
    let after = catalog
        .resolve(Provider::Openai, "o3", "2025-06-10", CommercialChannel::Api)
        .unwrap();
    let before_input = before.price().unwrap().resolved_rate_sets().unwrap()[0]
        .rate(RateDimension::Input, "tier=standard, region=global")
        .unwrap();
    let after_input = after.price().unwrap().resolved_rate_sets().unwrap()[0]
        .rate(RateDimension::Input, "tier=standard, region=global")
        .unwrap();

    assert_eq!(
        before_input.usd_per_million_tokens,
        Some(Decimal::new(10, 0))
    );
    assert_eq!(after_input.usd_per_million_tokens, Some(Decimal::new(2, 0)));
}

#[test]
fn cached_input_is_absent_before_openai_introduction() {
    let catalog = bundled_catalog().unwrap();

    let resolution = catalog
        .resolve(
            Provider::Openai,
            "gpt-4o-mini",
            "2024-09-30",
            CommercialChannel::Api,
        )
        .unwrap();
    let rate_sets = resolution.price().unwrap().resolved_rate_sets().unwrap();

    assert_eq!(rate_sets.len(), 1);
    assert_eq!(rate_sets[0].id, "oai-gpt4o-mini-launch");
}

#[test]
fn cached_input_is_separate_after_openai_introduction() {
    let catalog = bundled_catalog().unwrap();

    let resolution = catalog
        .resolve(
            Provider::Openai,
            "gpt-4o-mini",
            "2024-10-01",
            CommercialChannel::Api,
        )
        .unwrap();
    let rate_sets = resolution.price().unwrap().resolved_rate_sets().unwrap();

    assert_eq!(rate_sets.len(), 2);
    assert_eq!(rate_sets[0].id, "oai-gpt4o-mini-cache");
    assert_eq!(rate_sets[1].id, "oai-gpt4o-mini-launch");
}

#[test]
fn anthropic_cache_read_and_writes_remain_distinct_dimensions() {
    let catalog = bundled_catalog().unwrap();

    let resolution = catalog
        .resolve(
            Provider::Anthropic,
            "claude-sonnet-4-20250514",
            "2025-10-15",
            CommercialChannel::Api,
        )
        .unwrap();
    let rate_set = &resolution.price().unwrap().resolved_rate_sets().unwrap()[0];

    assert_eq!(
        rate_set
            .rate(
                RateDimension::CachedInput,
                "tier=standard, ttl=cache_read, region=global"
            )
            .unwrap()
            .usd_per_million_tokens,
        Some(Decimal::new(3, 1))
    );
    assert_eq!(
        rate_set
            .rate(
                RateDimension::CacheWrite,
                "tier=standard, ttl=5m, region=global"
            )
            .unwrap()
            .usd_per_million_tokens,
        Some(Decimal::new(375, 2))
    );
    assert_eq!(
        rate_set
            .rate(
                RateDimension::CacheWrite,
                "tier=standard, ttl=1h, region=global"
            )
            .unwrap()
            .usd_per_million_tokens,
        Some(Decimal::new(6, 0))
    );
}

#[test]
fn uncertain_launch_interval_remains_unknown_before_first_observation() {
    let catalog = bundled_catalog().unwrap();

    let before_observation = catalog
        .resolve(
            Provider::Openai,
            "gpt-3.5-turbo-0125",
            "2024-01-31",
            CommercialChannel::Api,
        )
        .unwrap();
    let first_observation = catalog
        .resolve(
            Provider::Openai,
            "gpt-3.5-turbo-0125",
            "2024-02-01",
            CommercialChannel::Api,
        )
        .unwrap();

    assert_eq!(before_observation, ReferenceResolution::Unknown);
    assert_eq!(
        first_observation
            .price()
            .unwrap()
            .resolved_rate_sets()
            .unwrap()[0]
            .interval,
        "observed 2024-02-01..open"
    );
}

#[test]
fn observation_bounded_price_transition_returns_ambiguity() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    assert_eq!(raw["rate_sets"][37]["id"], "oai-gpt56-sol-current");
    raw["rate_sets"][37]["window"]["precision"] = Value::String(String::from("observation_window"));
    raw["rate_sets"][37]["window"]["effective_from"] = Value::Null;
    raw["rate_sets"][37]["window"]["first_observed_new_rate"] =
        Value::String(String::from("2026-08-24"));
    let catalog = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap();

    let resolution = catalog
        .resolve(
            Provider::Openai,
            "gpt-5.6-sol",
            "2026-08-22",
            CommercialChannel::Api,
        )
        .unwrap();

    assert_eq!(
        resolution.price(),
        Some(&PriceResolution::TransitionAmbiguous {
            last_observed_old_rate: String::from("2026-08-20"),
            first_observed_new_rate: String::from("2026-08-24"),
            candidate_rate_set_ids: vec![
                String::from("oai-gpt56-sol-launch"),
                String::from("oai-gpt56-sol-current"),
            ],
        })
    );
}

#[test]
fn expired_rate_does_not_support_a_later_transition_observation() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    assert_eq!(raw["rate_sets"][36]["id"], "oai-gpt56-sol-launch");
    assert_eq!(raw["rate_sets"][37]["id"], "oai-gpt56-sol-current");
    raw["rate_sets"][36]["window"]["effective_until"] = Value::String(String::from("2026-08-01"));
    raw["rate_sets"][37]["window"]["precision"] = Value::String(String::from("observation_window"));
    raw["rate_sets"][37]["window"]["effective_from"] = Value::Null;
    raw["rate_sets"][37]["window"]["first_observed_new_rate"] =
        Value::String(String::from("2026-08-24"));
    let catalog = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap();

    let resolution = catalog
        .resolve(
            Provider::Openai,
            "gpt-5.6-sol",
            "2026-08-22",
            CommercialChannel::Api,
        )
        .unwrap();

    assert_eq!(resolution.price(), Some(&PriceResolution::Unknown));
}

#[test]
fn unrelated_rate_overlays_do_not_form_a_transition() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    assert_eq!(raw["rate_sets"][37]["id"], "oai-gpt56-sol-current");
    raw["rate_sets"][37]["window"]["precision"] = Value::String(String::from("observation_window"));
    raw["rate_sets"][37]["window"]["effective_from"] = Value::Null;
    raw["rate_sets"][37]["window"]["first_observed_new_rate"] =
        Value::String(String::from("2026-08-24"));
    raw["rate_sets"][37]["rates"][0]["qualifier"]["service_tier"] =
        Value::String(String::from("unrelated"));
    raw["rate_sets"][37]["rates"][1]["qualifier"]["service_tier"] =
        Value::String(String::from("unrelated"));
    raw["rate_sets"][37]["rates"][2]["qualifier"]["service_tier"] =
        Value::String(String::from("unrelated"));
    raw["rate_sets"][37]["rates"][3]["qualifier"]["service_tier"] =
        Value::String(String::from("unrelated"));
    let catalog = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap();

    let resolution = catalog
        .resolve(
            Provider::Openai,
            "gpt-5.6-sol",
            "2026-08-22",
            CommercialChannel::Api,
        )
        .unwrap();

    assert_eq!(resolution.price(), Some(&PriceResolution::Unknown));
}

#[test]
fn exact_and_family_consumer_mappings_do_not_conflate() {
    let catalog = bundled_catalog().unwrap();

    let exact = catalog
        .resolve(
            Provider::Anthropic,
            "claude-sonnet-4-5-20250929",
            "2025-10-15",
            CommercialChannel::ClaudeCodeSubscription,
        )
        .unwrap();
    let family = catalog
        .resolve(
            Provider::Anthropic,
            "sonnet",
            "2025-10-15",
            CommercialChannel::ClaudeCodeSubscription,
        )
        .unwrap();

    assert_eq!(
        exact.resolved_mapping_quality(),
        Some(MappingQuality::Exact)
    );
    assert_eq!(family.resolved_model_id(), None);
}

#[test]
fn claude_code_sonnet_alias_moves_to_claude5_family_at_launch() {
    let catalog = bundled_catalog().unwrap();

    let before = catalog
        .resolve(
            Provider::Anthropic,
            "sonnet",
            "2026-06-29",
            CommercialChannel::ClaudeCodeSubscription,
        )
        .unwrap();
    let after = catalog
        .resolve(
            Provider::Anthropic,
            "sonnet",
            "2026-06-30",
            CommercialChannel::ClaudeCodeSubscription,
        )
        .unwrap();

    assert_eq!(
        resolved_family_id(&before),
        Some("anthropic:claude-4-family")
    );
    assert_eq!(
        resolved_family_id(&after),
        Some("anthropic:claude-5-family")
    );
}

#[test]
fn claude_code_opus_alias_moves_to_claude5_family_at_launch() {
    let catalog = bundled_catalog().unwrap();

    let before = catalog
        .resolve(
            Provider::Anthropic,
            "opus",
            "2026-07-23",
            CommercialChannel::ClaudeCodeSubscription,
        )
        .unwrap();
    let after = catalog
        .resolve(
            Provider::Anthropic,
            "opus",
            "2026-07-24",
            CommercialChannel::ClaudeCodeSubscription,
        )
        .unwrap();

    assert_eq!(
        resolved_family_id(&before),
        Some("anthropic:claude-4-family")
    );
    assert_eq!(
        resolved_family_id(&after),
        Some("anthropic:claude-5-family")
    );
}

#[test]
fn claude_consumer_sonnet_label_moves_to_claude5_family_at_launch() {
    let catalog = bundled_catalog().unwrap();

    let before = catalog
        .resolve(
            Provider::Anthropic,
            "Sonnet",
            "2026-06-29",
            CommercialChannel::ClaudeSubscription,
        )
        .unwrap();
    let after = catalog
        .resolve(
            Provider::Anthropic,
            "Sonnet",
            "2026-06-30",
            CommercialChannel::ClaudeSubscription,
        )
        .unwrap();

    assert_eq!(
        resolved_family_id(&before),
        Some("anthropic:claude-4-family")
    );
    assert_eq!(
        resolved_family_id(&after),
        Some("anthropic:claude-5-family")
    );
}

#[test]
fn competing_consumer_mappings_return_ambiguity() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    let mut competing = raw["consumer_mappings"][0].clone();
    assert_eq!(competing["id"], "oai-chatgpt-gpt35");
    competing["id"] = Value::String(String::from("competing-family"));
    competing["normalized_model"] = Value::String(String::from("openai:gpt-4-family"));
    competing["window"]["effective_from"] = Value::String(String::from("2023-03-14"));
    competing["window"]["first_observed_new_rate"] = Value::String(String::from("2023-03-14"));
    raw["consumer_mappings"]
        .as_array_mut()
        .unwrap()
        .push(competing);
    let catalog = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap();

    let resolution = catalog
        .resolve(
            Provider::Openai,
            "GPT-3.5",
            "2024-01-01",
            CommercialChannel::ChatgptSubscription,
        )
        .unwrap();

    assert_eq!(
        resolution,
        ReferenceResolution::Ambiguous {
            candidate_model_ids: vec![
                String::from("openai:gpt-3.5-family"),
                String::from("openai:gpt-4-family"),
            ],
        }
    );
}

#[test]
fn missing_price_is_unknown_not_zero() {
    let catalog = bundled_catalog().unwrap();

    let resolution = catalog
        .resolve(
            Provider::Openai,
            "gpt-5.3-codex",
            "2026-08-24",
            CommercialChannel::Api,
        )
        .unwrap();

    assert_eq!(resolution.resolved_model_id(), Some("openai:gpt-5.3-codex"));
    assert_eq!(resolution.price(), Some(&PriceResolution::Unknown));
}

#[test]
fn query_after_the_evidence_horizon_is_unknown() {
    let catalog = bundled_catalog().unwrap();

    let resolution = catalog
        .resolve(
            Provider::Openai,
            "gpt-5.6-sol",
            "2026-09-04",
            CommercialChannel::Api,
        )
        .unwrap();

    assert_eq!(resolution, ReferenceResolution::Unknown);
}

#[test]
fn malformed_catalog_field_is_rejected() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    raw["unsupported_authority"] = Value::Bool(true);

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(error.to_string().contains("unsupported_authority"));
}

#[test]
fn catalog_dates_require_strict_calendar_dates() {
    for date in ["2025-02-29", "2024-2-29", "2024-02-30"] {
        let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
        raw["verified_through"]["openai"] = Value::String(String::from(date));

        let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

        assert!(
            error.to_string().contains("YYYY-MM-DD calendar date"),
            "{date}: {error}"
        );
    }
}

#[test]
fn rate_window_cannot_extend_past_model_availability() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    assert_eq!(raw["rate_sets"][4]["id"], "oai-gpt4-launch");
    raw["rate_sets"][4]["window"]["effective_until"] = Value::Null;

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("extends beyond model availability")
    );
}

#[test]
fn consumer_mapping_cannot_outlive_model_availability() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    assert_eq!(raw["consumer_mappings"][9]["id"], "oai-codex-cli-mini");
    raw["consumer_mappings"][9]["observed_identity"] =
        Value::String(String::from("gpt-3.5-turbo-0301"));
    raw["consumer_mappings"][9]["normalized_model"] =
        Value::String(String::from("openai:gpt-3.5-turbo-0301"));
    raw["consumer_mappings"][9]["window"]["effective_from"] =
        Value::String(String::from("2023-03-01"));
    raw["consumer_mappings"][9]["window"]["first_observed_new_rate"] =
        Value::String(String::from("2023-03-01"));

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("extends beyond model availability")
    );
}

#[test]
fn consumer_mapping_cannot_start_after_model_retirement() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    assert_eq!(raw["consumer_mappings"][9]["id"], "oai-codex-cli-mini");
    raw["consumer_mappings"][9]["observed_identity"] =
        Value::String(String::from("gpt-3.5-turbo-0301"));
    raw["consumer_mappings"][9]["normalized_model"] =
        Value::String(String::from("openai:gpt-3.5-turbo-0301"));
    raw["consumer_mappings"][9]["window"]["effective_from"] =
        Value::String(String::from("2025-05-01"));
    raw["consumer_mappings"][9]["window"]["effective_until"] =
        Value::String(String::from("2025-06-01"));
    raw["consumer_mappings"][9]["window"]["first_observed_new_rate"] =
        Value::String(String::from("2025-05-01"));

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("extends beyond model availability")
    );
}

#[test]
fn exact_day_observation_boundary_must_be_ordered() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    assert_eq!(raw["rate_sets"][15]["id"], "oai-o3-reduced");
    raw["rate_sets"][15]["window"]["last_observed_old_rate"] =
        Value::String(String::from("2025-06-11"));

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("does not leave an ordered observation boundary")
    );
}

#[test]
fn consumer_mapping_observation_boundary_must_be_ordered() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    consumer_mapping_mut(&mut raw, "anth-code-default-sonnet5").unwrap()["window"]["last_observed_old_rate"] =
        Value::String(String::from("2026-06-30"));

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("does not leave an ordered observation boundary")
    );
}

#[test]
fn consumer_channel_must_belong_to_the_mapping_provider() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    assert_eq!(raw["consumer_mappings"][0]["id"], "oai-chatgpt-gpt35");
    raw["consumer_mappings"][0]["commercial_channel"] =
        Value::String(String::from("claude_subscription"));

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("another provider's commercial channel")
    );
}

#[test]
fn low_confidence_rate_requires_an_explicit_limitation() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    assert_eq!(raw["rate_sets"][0]["id"], "oai-gpt35-launch");
    raw["rate_sets"][0]["confidence"] = Value::String(String::from("low"));
    raw["rate_sets"][0]["limitations"] = Value::Array(Vec::new());

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(error.to_string().contains("has no explicit limitation"));
}

#[test]
fn low_confidence_mapping_requires_an_explicit_limitation() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    assert_eq!(raw["consumer_mappings"][0]["id"], "oai-chatgpt-gpt35");
    raw["consumer_mappings"][0]["confidence"] = Value::String(String::from("low"));
    raw["consumer_mappings"][0]["limitations"] = Value::Array(Vec::new());

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(error.to_string().contains("has no explicit limitation"));
}

#[test]
fn projection_breaking_source_text_is_rejected() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    assert_eq!(raw["sources"][0]["id"], "oai-gpt35-launch-2023-03-01");
    raw["sources"][0]["evidence"] = Value::String(String::from("safe\n```\n# injected"));

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(error.to_string().contains("projection-breaking character"));
}

#[test]
fn source_ownership_uses_the_canonicalized_url_path() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    let source = raw["sources"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|source| source["id"] == "oai-codex-model-catalog-2026-09-03")
        .unwrap();
    source["url"] = Value::String(String::from("https://github.com/openai/../attacker/repo"));

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("not a recognized first-party URL")
    );
}

#[test]
fn concrete_model_requires_a_provider_spelling() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    assert_eq!(raw["models"][1]["id"], "openai:gpt-3.5-turbo");
    raw["models"][1]["provider_model_id"] = Value::Null;

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(error.to_string().contains("requires a provider spelling"));
}

#[test]
fn family_reference_must_target_a_family_record() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    assert_eq!(raw["models"][1]["id"], "openai:gpt-3.5-turbo");
    raw["models"][1]["family"] = Value::String(String::from("openai:gpt-3.5-turbo-0301"));

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("family reference does not target a family")
    );
}

#[test]
fn contradictory_capability_records_are_rejected() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    let mut contradiction = raw["models"][1]["capabilities"][0].clone();
    assert_eq!(contradiction["capability"], "text_input_output");
    contradiction["support"] = Value::String(String::from("unsupported"));
    raw["models"][1]["capabilities"]
        .as_array_mut()
        .unwrap()
        .push(contradiction);

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(error.to_string().contains("repeats capability"));
}

#[test]
fn qualifier_values_cannot_collide_with_rendered_lookup_delimiters() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    assert_eq!(raw["rate_sets"][0]["id"], "oai-gpt35-launch");
    raw["rate_sets"][0]["rates"][0]["qualifier"]["service_tier"] =
        Value::String(String::from("standard, context=large"));

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(error.to_string().contains("reserved delimiter"));
}

#[test]
fn exact_mapping_must_use_the_target_provider_spelling() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    assert_eq!(raw["consumer_mappings"][9]["id"], "oai-codex-cli-mini");
    raw["consumer_mappings"][9]["observed_identity"] = Value::String(String::from("default"));

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("does not use the target model's provider spelling")
    );
}

#[test]
fn incompatible_overlapping_rate_is_rejected() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    let mut overlap = raw["rate_sets"][0].clone();
    assert_eq!(overlap["id"], "oai-gpt35-launch");
    overlap["id"] = Value::String(String::from("incompatible-overlap"));
    overlap["rates"][0]["usd_per_million_tokens"] = Value::String(String::from("3"));
    overlap["rates"][0]["original"]["amount"] = Value::String(String::from("0.003"));
    raw["rate_sets"].as_array_mut().unwrap().push(overlap);

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("incompatible overlapping rate sets")
    );
}

#[test]
fn inv_077_reference_catalog_has_no_workspace_dependency_edge() {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();
    let packages = metadata["packages"].as_array().unwrap();
    let workspace_packages = packages
        .iter()
        .map(|package| package["name"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    let mut dependency_edges = packages
        .iter()
        .flat_map(|package| {
            let package_name = package["name"].as_str().unwrap();
            let workspace_packages = &workspace_packages;
            package["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(move |dependency| {
                    let dependency_name = dependency["name"].as_str().unwrap();
                    (workspace_packages.contains(dependency_name)
                        && (package_name == REFERENCE_PACKAGE
                            || dependency_name == REFERENCE_PACKAGE))
                        .then(|| format!("{package_name} -> {dependency_name}"))
                })
        })
        .collect::<Vec<_>>();
    dependency_edges.sort();

    assert_eq!(dependency_edges, Vec::<String>::new());
}

#[test]
fn appended_future_rate_does_not_change_earlier_resolution() {
    let baseline = bundled_catalog().unwrap();
    let before = baseline
        .resolve(
            Provider::Openai,
            "gpt-3.5-turbo",
            "2023-03-10",
            CommercialChannel::Api,
        )
        .unwrap();
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    let mut future = raw["rate_sets"][0].clone();
    assert_eq!(future["id"], "oai-gpt35-launch");
    future["id"] = Value::String(String::from("oai-gpt35-future-tier"));
    future["window"]["effective_from"] = Value::String(String::from("2030-01-01"));
    future["window"]["effective_until"] = Value::Null;
    future["window"]["first_observed_new_rate"] = Value::String(String::from("2030-01-01"));
    future["rates"][0]["qualifier"]["service_tier"] = Value::String(String::from("future"));
    raw["rate_sets"].as_array_mut().unwrap().push(future);
    let appended = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap();

    let after = appended
        .resolve(
            Provider::Openai,
            "gpt-3.5-turbo",
            "2023-03-10",
            CommercialChannel::Api,
        )
        .unwrap();

    assert_eq!(after, before);
}

#[test]
fn checked_in_inspection_tables_match_canonical_data() {
    let catalog = bundled_catalog().unwrap();

    let projections = render_projections(&catalog);

    assert_eq!(projections[0].filename, "consumer-equivalence.md");
    assert_eq!(
        projections[0].contents,
        include_str!("../projections/consumer-equivalence.md")
    );
    assert_eq!(projections[1].filename, "historical-pricing.md");
    assert_eq!(
        projections[1].contents,
        include_str!("../projections/historical-pricing.md")
    );
    assert_eq!(projections[2].filename, "models.md");
    assert_eq!(
        projections[2].contents,
        include_str!("../projections/models.md")
    );
    assert_eq!(projections[3].filename, "research-gaps.md");
    assert_eq!(
        projections[3].contents,
        include_str!("../projections/research-gaps.md")
    );
    assert_eq!(projections[4].filename, "sources.md");
    assert_eq!(
        projections[4].contents,
        include_str!("../projections/sources.md")
    );
}
