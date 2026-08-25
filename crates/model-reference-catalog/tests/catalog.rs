use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_model_reference_catalog::{
    ActualBillingKind, BUNDLED_CATALOG_JSON, Catalog, CommercialChannel, MappingQuality,
    PriceResolution, Provider, RateDimension, ReferenceResolution, bundled_catalog,
    render_projections,
};

fn exact_api_snapshot_rate_set_ids(
    catalog: &Catalog,
    model_hint: &str,
    date: &str,
) -> Option<Vec<String>> {
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
        exact_api_snapshot_rate_set_ids(&catalog, "gpt-4-0314", "2023-03-14"),
        Some(vec![String::from("oai-gpt4-launch")])
    );
    assert_eq!(
        exact_api_snapshot_rate_set_ids(&catalog, "gpt-4-32k-0314", "2023-03-14"),
        Some(vec![String::from("oai-gpt4-32k-launch")])
    );
    assert_eq!(
        exact_api_snapshot_rate_set_ids(&catalog, "gpt-4-1106-preview", "2023-11-06"),
        Some(vec![String::from("oai-gpt4-turbo-launch")])
    );
    assert_eq!(
        exact_api_snapshot_rate_set_ids(&catalog, "gpt-4o-2024-08-06", "2024-10-01"),
        Some(vec![String::from("oai-gpt4o-0806-caching")])
    );
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

    assert_eq!(before_observation.price(), Some(&PriceResolution::Unknown));
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
fn malformed_catalog_field_is_rejected() {
    let mut raw: Value = serde_json::from_str(BUNDLED_CATALOG_JSON).unwrap();
    raw["unsupported_authority"] = Value::Bool(true);

    let error = Catalog::from_json(&serde_json::to_string(&raw).unwrap()).unwrap_err();

    assert!(error.to_string().contains("unsupported_authority"));
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
fn reference_catalog_has_no_runtime_authority_dependency() {
    let reference_manifest = include_str!("../Cargo.toml");
    let daemon_manifest = include_str!("../../../apps/signalboxd/Cargo.toml");

    assert!(!reference_manifest.contains("signalbox-domain"));
    assert!(!reference_manifest.contains("signalbox-model-provider-runtime"));
    assert!(!reference_manifest.contains("signalbox-model-runtime"));
    assert!(!daemon_manifest.contains("signalbox-model-reference-catalog"));
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
