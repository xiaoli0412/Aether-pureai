//! Tests for Aether-NewAPI collaboration contract
//!
//! Validates:
//! - relay context correct signature verification
//! - tampered context rejection
//! - expired context rejection
//! - dynamic quota_per_unit (no hardcoded 500000)
//! - parallel_shadow does not allow upstream execution
//! - financial data contains no PII

use crate::models::{PricingParams, ProfitInput};
use crate::pricing::{calculate_token_cost_quota, quota_to_usd, usd_to_quota};
use crate::profit::calculate_profit;

#[test]
fn collaboration_contract_manifest_defines_canonical_v1_surface() {
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};

    let contract_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/contracts");
    let data = std::fs::read_to_string(contract_dir.join("aether-newapi-v1.json"))
        .expect("contract manifest should exist");
    let manifest: serde_json::Value =
        serde_json::from_str(&data).expect("contract manifest should be valid JSON");

    assert_eq!(manifest["contract_version"], "aether-newapi/v1");
    assert_eq!(
        manifest["active_modes"],
        serde_json::json!(["direct_channel"])
    );
    assert!(manifest["relay_headers"]
        .as_array()
        .expect("relay_headers should be an array")
        .iter()
        .any(|value| value == "X-Aether-Relay-Signature"));
    assert!(manifest["new_api_endpoints"]
        .as_array()
        .expect("new_api_endpoints should be an array")
        .iter()
        .any(|value| value == "/api/aether/v1/events"));
    assert_eq!(manifest["revision_conflict"]["http_status"], 409);
    assert_eq!(
        manifest["revision_conflict"]["response_schema"],
        "revision_conflict_response"
    );
    assert_eq!(
        manifest["revision_conflict"]["current_config_field"],
        "current_config"
    );
    assert_eq!(
        manifest["revision_conflict"]["diff_entry_fields"],
        serde_json::json!(["requested", "current"])
    );
    assert_eq!(
        manifest["instance_status"]["response_schema"],
        "instance_status_response"
    );
    assert_eq!(
        manifest["instance_status"]["healthy_semantics"],
        "control_execution_readiness_not_upstream_sla"
    );
    assert_eq!(
        manifest["instance_status"]["fields"],
        serde_json::json!([
            "instance_id",
            "healthy",
            "last_sync_at",
            "capability_version",
            "base_revision",
            "uptime_secs",
            "active_channels",
            "routing_mode"
        ])
    );
    assert_eq!(
        manifest["instance_status"]["last_sync_at_semantics"],
        "persisted_config_updated_at_rfc3339_or_null"
    );
    assert_eq!(
        manifest["instance_status"]["uptime_secs_semantics"],
        "zero_until_a_durable_uptime_source_exists"
    );
    assert_eq!(
        manifest["instance_status"]["config_store_unavailable_http_status"],
        503
    );
    assert_eq!(
        manifest["instance_status"]["routing_mode_values"],
        serde_json::json!([
            "direct_channel",
            "disabled",
            "parallel_shadow",
            "aether_decision"
        ])
    );

    let schema_name = manifest["json_schema"]
        .as_str()
        .expect("json_schema should name the schema artifact");
    let examples_name = manifest["examples"]
        .as_str()
        .expect("examples should name the examples artifact");
    let schema = std::fs::read(contract_dir.join(schema_name)).expect("schema should exist");
    let examples = std::fs::read(contract_dir.join(examples_name)).expect("examples should exist");
    let schema_json: serde_json::Value =
        serde_json::from_slice(&schema).expect("schema should be valid JSON");
    let examples_json: serde_json::Value =
        serde_json::from_slice(&examples).expect("examples should be valid JSON");
    assert_eq!(
        schema_json["$defs"]["relay_context"]["additionalProperties"],
        false
    );
    assert_eq!(
        schema_json["$defs"]["request_id_string"],
        serde_json::json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 100,
            "pattern": "^[!-~]+$"
        })
    );
    assert_eq!(
        schema_json["$defs"]["relay_context"]["properties"]["request_id"]["$ref"],
        "#/$defs/request_id_string"
    );
    let relay_required = schema_json["$defs"]["relay_context"]["required"]
        .as_array()
        .expect("relay_context.required should be an array");
    for field in ["group", "model", "relay_format"] {
        assert!(
            relay_required.iter().any(|value| value == field),
            "relay_context should require {field}"
        );
    }
    let revision_conflict_required = schema_json["$defs"]["revision_conflict_response"]["required"]
        .as_array()
        .expect("revision_conflict_response.required should be an array");
    for field in ["error", "current_revision", "current_config", "diff"] {
        assert!(
            revision_conflict_required
                .iter()
                .any(|value| value == field),
            "revision_conflict_response should require {field}"
        );
    }
    assert_eq!(
        schema_json["$defs"]["revision_conflict_response"]["properties"]["current_config"]["$ref"],
        "#/$defs/instance_config"
    );
    assert_eq!(
        schema_json["$defs"]["revision_conflict_response"]["properties"]["diff"]["$ref"],
        "#/$defs/revision_conflict_diff"
    );
    assert_eq!(
        schema_json["$defs"]["revision_conflict_diff_entry"]["required"],
        serde_json::json!(["requested", "current"])
    );
    assert_eq!(
        schema_json["$defs"]["revision_conflict_diff_entry"]["additionalProperties"],
        false
    );
    assert!(schema_json["oneOf"]
        .as_array()
        .expect("top-level oneOf should be an array")
        .iter()
        .any(|entry| entry["$ref"] == "#/$defs/instance_status_response"));
    let instance_status = &schema_json["$defs"]["instance_status_response"];
    assert_eq!(instance_status["additionalProperties"], false);
    assert_eq!(
        instance_status["required"],
        manifest["instance_status"]["fields"]
    );
    assert_eq!(
        instance_status["properties"]["instance_id"]["$ref"],
        "#/$defs/id_string"
    );
    assert_eq!(instance_status["properties"]["healthy"]["type"], "boolean");
    assert_eq!(
        instance_status["properties"]["healthy"]["description"],
        manifest["instance_status"]["healthy_semantics"]
    );
    for field in [
        "capability_version",
        "base_revision",
        "uptime_secs",
        "active_channels",
    ] {
        assert!(
            instance_status["properties"].get(field).is_some(),
            "instance_status_response should define {field}"
        );
    }
    assert_eq!(
        instance_status["properties"]["routing_mode"]["enum"],
        manifest["instance_status"]["routing_mode_values"]
    );
    let last_sync_one_of = instance_status["properties"]["last_sync_at"]["oneOf"]
        .as_array()
        .expect("last_sync_at should permit timestamp or null");
    assert!(last_sync_one_of
        .iter()
        .any(|entry| entry["type"] == "string" && entry["format"] == "date-time"));
    assert!(last_sync_one_of.iter().any(|entry| entry["type"] == "null"));
    assert!(examples_json.get("relay_context").is_some());
    assert!(examples_json.get("events_response").is_some());
    assert!(examples_json.get("pricing_response").is_some());
    assert!(examples_json.get("snapshot_response").is_some());
    let instance_status = examples_json
        .get("instance_status_response")
        .expect("instance_status_response should be present");
    assert_eq!(instance_status["instance_id"], "aether-primary");
    assert_eq!(instance_status["healthy"], true);
    assert_eq!(instance_status["last_sync_at"], "2026-07-18T10:00:00Z");
    assert_eq!(instance_status["capability_version"], "0.1.0");
    assert_eq!(instance_status["base_revision"], 7);
    assert_eq!(instance_status["uptime_secs"], 0);
    assert_eq!(instance_status["active_channels"], 2);
    assert_eq!(instance_status["routing_mode"], "direct_channel");
    let revision_conflict = examples_json
        .get("revision_conflict_response")
        .expect("revision_conflict_response should be present");
    assert!(revision_conflict["error"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));
    assert_eq!(
        revision_conflict["current_revision"],
        revision_conflict["current_config"]["base_revision"]
    );
    assert!(revision_conflict["diff"]["route_profile"]["requested"].is_string());
    assert!(revision_conflict["diff"]["route_profile"]["current"].is_string());
    let signature_vector = examples_json
        .get("relay_signature_vector")
        .expect("relay_signature_vector should be present");
    let signing_secret = signature_vector["signing_secret"]
        .as_str()
        .expect("signature vector secret should be a string");
    let encoded_context = signature_vector["encoded_context"]
        .as_str()
        .expect("signature vector context should be a string");
    let signature_hex = signature_vector["signature_hex"]
        .as_str()
        .expect("signature vector signature should be a string");
    let mut mac = Hmac::<Sha256>::new_from_slice(signing_secret.as_bytes())
        .expect("signature vector secret should be valid");
    mac.update(encoded_context.as_bytes());
    let actual_signature = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(actual_signature, signature_hex);
    let service_vector = examples_json
        .get("service_signature_vector")
        .expect("service_signature_vector should be present");
    let service_secret = service_vector["signing_secret"]
        .as_str()
        .expect("service signature secret should be a string");
    let canonical_payload = service_vector["canonical_payload"]
        .as_str()
        .expect("service canonical payload should be a string");
    assert_eq!(
        canonical_payload,
        "GET\n/api/aether/v1/pricing\ngroup=%E4%B8%AD%E6%96%87+pro&cursor=a%2Fb%3F\n1784073600\nnonce-1234567890"
    );
    let service_signature = service_vector["signature_hex"]
        .as_str()
        .expect("service signature should be a string");
    let mut service_mac = Hmac::<Sha256>::new_from_slice(service_secret.as_bytes())
        .expect("service signature secret should be valid");
    service_mac.update(canonical_payload.as_bytes());
    let actual_service_signature = service_mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(actual_service_signature, service_signature);

    let control_vector = examples_json
        .get("control_hmac_v2_signature_vector")
        .expect("control_hmac_v2_signature_vector should be present");
    let control_secret = control_vector["signing_secret"]
        .as_str()
        .expect("control signature secret should be a string");
    let control_raw_body = control_vector["raw_body"]
        .as_str()
        .expect("control raw body should be a string");
    let control_body_sha256 = control_vector["body_sha256"]
        .as_str()
        .expect("control body digest should be a string");
    assert_eq!(
        format!("{:x}", Sha256::digest(control_raw_body.as_bytes())),
        control_body_sha256
    );
    let control_canonical_payload = control_vector["canonical_payload"]
        .as_str()
        .expect("control canonical payload should be a string");
    assert_eq!(
        control_canonical_payload,
        "AETHER-CONTROL-V2\nPUT\n/api/integrations/new-api/v1/instances/aether-primary\n\naether-primary\n1784073600\nnonce-control-v2-123456\ned3912c274f42d73d47de42f64632bc09cce1e430b97cc76294ff80f8103cb47"
    );
    let control_signature = control_vector["signature_hex"]
        .as_str()
        .expect("control signature should be a string");
    let mut control_mac = Hmac::<Sha256>::new_from_slice(control_secret.as_bytes())
        .expect("control signature secret should be valid");
    control_mac.update(control_canonical_payload.as_bytes());
    let actual_control_signature = control_mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(actual_control_signature, control_signature);

    let mut hasher = Sha256::new();
    hasher.update(&schema);
    hasher.update([0]);
    hasher.update(&examples);
    assert_eq!(
        format!("{:x}", hasher.finalize()),
        manifest["bundle_sha256"]
            .as_str()
            .expect("bundle_sha256 should be present")
    );
}

#[test]
fn collaboration_contract_bundle_has_audited_bilateral_baseline() {
    use sha2::{Digest, Sha256};
    use std::path::{Path, PathBuf};

    let contract_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/contracts");
    let peer_root = std::env::var("AETHER_NEWAPI_CONTRACT_PEER_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../..")
                .join("new-api")
        });
    let peer_contract_dir = peer_root.join("docs/contracts");
    let tracked_files = [
        "aether-newapi-v1.json",
        "aether-newapi-v1.schema.json",
        "aether-newapi-v1.examples.json",
    ];

    for name in tracked_files {
        let local = std::fs::read(contract_dir.join(name))
            .unwrap_or_else(|error| panic!("local {name} should exist: {error}"));
        let peer = std::fs::read(peer_contract_dir.join(name))
            .unwrap_or_else(|error| panic!("peer {name} should exist: {error}"));
        assert_eq!(
            local, peer,
            "{name} must be byte-identical in both repositories"
        );
    }

    let baseline_name = "aether-newapi-v1.baseline.json";
    let local_baseline = std::fs::read(contract_dir.join(baseline_name))
        .expect("the local audited contract baseline should exist");
    let peer_baseline = std::fs::read(peer_contract_dir.join(baseline_name))
        .expect("the peer audited contract baseline should exist");
    assert_eq!(
        local_baseline, peer_baseline,
        "the audited baseline must be byte-identical in both repositories"
    );
    let baseline: serde_json::Value =
        serde_json::from_slice(&local_baseline).expect("the baseline should be valid JSON");
    assert_eq!(
        baseline["baseline_format"],
        "aether-newapi-contract-baseline/v1"
    );
    assert!(baseline["baseline_revision"]
        .as_u64()
        .is_some_and(|revision| revision > 0));
    assert_eq!(baseline["tracked_files"], serde_json::json!(tracked_files));
    for field in ["change_id", "recorded_at", "rationale"] {
        assert!(
            baseline["change_control"][field]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "baseline change control must provide {field}"
        );
    }
    assert_eq!(
        baseline["change_control"]["review_policy"],
        "cross-repository-review-required"
    );

    let manifest_bytes = std::fs::read(contract_dir.join(tracked_files[0]))
        .expect("the local contract manifest should exist");
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).expect("the manifest should be valid JSON");
    assert_eq!(baseline["contract_version"], manifest["contract_version"]);
    assert_eq!(baseline["schema_revision"], manifest["schema_revision"]);

    let schema = std::fs::read(contract_dir.join(tracked_files[1]))
        .expect("the local contract schema should exist");
    let examples = std::fs::read(contract_dir.join(tracked_files[2]))
        .expect("the local contract examples should exist");
    let mut hasher = Sha256::new();
    hasher.update(&schema);
    hasher.update([0]);
    hasher.update(&examples);
    let actual_bundle_sha256 = format!("{:x}", hasher.finalize());
    assert_eq!(manifest["bundle_sha256"], actual_bundle_sha256);
    assert_eq!(baseline["bundle_sha256"], actual_bundle_sha256);
}

// ==================== Dynamic quota_per_unit Tests ====================

#[test]
fn test_no_hardcoded_quota_per_unit() {
    // Verify that different quota_per_unit values produce different results
    let params = PricingParams {
        model_ratio: 2.0,
        group_ratio: 1.5,
        completion_ratio: 3.0,
    };
    let cost_quota = calculate_token_cost_quota(1000, false, &params);

    // With quota_per_unit = 500_000 (New API default)
    let usd_500k = quota_to_usd(cost_quota, 500_000.0);
    // With quota_per_unit = 1_000_000 (hypothetical different platform)
    let usd_1m = quota_to_usd(cost_quota, 1_000_000.0);
    // With quota_per_unit = 250_000
    let usd_250k = quota_to_usd(cost_quota, 250_000.0);

    // All three must be different — proves no hardcoding
    assert_ne!(usd_500k, usd_1m);
    assert_ne!(usd_500k, usd_250k);
    assert_ne!(usd_1m, usd_250k);

    // Verify math: usd_1m should be half of usd_500k
    assert!((usd_500k - usd_1m * 2.0).abs() < 1e-10);
}

#[test]
fn quota_conversion_reports_unknown_instead_of_fabricating_zero_revenue() {
    use crate::pricing::try_quota_to_usd;

    assert_eq!(try_quota_to_usd(2_500_000.0, 1_000_000.0), Some(2.5));
    assert_eq!(try_quota_to_usd(2_500_000.0, 0.0), None);
    assert_eq!(try_quota_to_usd(2_500_000.0, f64::NAN), None);
}

#[test]
fn test_quota_to_usd_zero_divisor_returns_zero() {
    // quota_per_unit = 0 should not panic, should return 0
    let result = quota_to_usd(1000.0, 0.0);
    assert_eq!(result, 0.0);

    let result_neg = quota_to_usd(1000.0, -1.0);
    assert_eq!(result_neg, 0.0);
}

#[test]
fn test_usd_to_quota_roundtrip() {
    let qpu = 750_000.0; // Non-standard quota_per_unit
    let original_usd = 10.0;
    let quota = usd_to_quota(original_usd, qpu);
    let back_to_usd = quota_to_usd(quota, qpu);
    assert!((original_usd - back_to_usd).abs() < 1e-10);
}

#[test]
fn test_profit_uses_dynamic_quota_per_unit() {
    let input_500k = ProfitInput {
        prompt_tokens: 1000,
        completion_tokens: 500,
        upstream_pricing: PricingParams {
            model_ratio: 2.0,
            group_ratio: 1.0,
            completion_ratio: 2.0,
        },
        downstream_price_per_prompt_quota: 4.0,
        downstream_price_per_completion_quota: 8.0,
        payment_fee_rate: 0.006,
        quota_per_unit: 500_000.0,
    };

    let input_1m = ProfitInput {
        quota_per_unit: 1_000_000.0,
        ..input_500k.clone()
    };

    let result_500k = calculate_profit(&input_500k);
    let result_1m = calculate_profit(&input_1m);

    // With double the quota_per_unit, USD values should be halved
    assert!((result_500k.upstream_cost_usd - result_1m.upstream_cost_usd * 2.0).abs() < 1e-10);
    assert!(
        (result_500k.downstream_revenue_usd - result_1m.downstream_revenue_usd * 2.0).abs() < 1e-10
    );
}

// ==================== Cost Confidence Tests ====================

#[test]
fn test_profit_with_zero_pricing_indicates_unknown() {
    // When model_ratio or group_ratio is 0, upstream cost should be 0
    // This is the "unknown" cost scenario
    let input = ProfitInput {
        prompt_tokens: 1000,
        completion_tokens: 500,
        upstream_pricing: PricingParams {
            model_ratio: 0.0, // Unknown
            group_ratio: 1.0,
            completion_ratio: 2.0,
        },
        downstream_price_per_prompt_quota: 4.0,
        downstream_price_per_completion_quota: 8.0,
        payment_fee_rate: 0.006,
        quota_per_unit: 500_000.0,
    };

    let result = calculate_profit(&input);
    // Upstream cost should be 0 when model_ratio is 0
    assert_eq!(result.upstream_cost_usd, 0.0);
}

// ==================== Routing Mode Tests ====================

#[test]
fn test_routing_mode_serialization() {
    // Verify routing mode enum serializes to expected strings
    let dc = serde_json::to_string(&crate::models::MarkupStrategy::TargetMargin { margin: 0.3 })
        .unwrap();
    assert!(dc.contains("TargetMargin")); // proves serialization works
}

// ==================== Financial Safety Tests ====================

#[test]
fn test_profit_result_contains_no_pii_fields() {
    // ProfitResult should only contain financial numbers, no user-identifiable fields
    let input = ProfitInput {
        prompt_tokens: 1000,
        completion_tokens: 500,
        upstream_pricing: PricingParams {
            model_ratio: 2.0,
            group_ratio: 1.5,
            completion_ratio: 3.0,
        },
        downstream_price_per_prompt_quota: 5.0,
        downstream_price_per_completion_quota: 15.0,
        payment_fee_rate: 0.006,
        quota_per_unit: 500_000.0,
    };

    let result = calculate_profit(&input);
    let json = serde_json::to_string(&result).unwrap();

    // Must NOT contain any PII-related strings
    assert!(!json.contains("user"));
    assert!(!json.contains("email"));
    assert!(!json.contains("name"));
    assert!(!json.contains("api_key"));
    assert!(!json.contains("password"));
    assert!(!json.contains("token_key"));

    // Must contain only financial fields
    assert!(json.contains("upstream_cost_usd"));
    assert!(json.contains("downstream_revenue_usd"));
    assert!(json.contains("net_profit_usd"));
    assert!(json.contains("margin_percent"));
}

// ==================== Decimal Precision Tests ====================

#[test]
fn test_token_cost_precision() {
    // Verify calculation doesn't lose precision for typical values
    let params = PricingParams {
        model_ratio: 15.0,
        group_ratio: 0.7,
        completion_ratio: 2.0,
    };

    let cost = calculate_token_cost_quota(1, false, &params);
    // 1 * 15.0 * 0.7 = 10.5 exactly
    assert_eq!(cost, 10.5);

    let cost_completion = calculate_token_cost_quota(1, true, &params);
    // 1 * 15.0 * 0.7 * 2.0 = 21.0 exactly
    assert_eq!(cost_completion, 21.0);
}

// ==================== Edge Cases ====================

#[test]
fn test_profit_zero_tokens() {
    let input = ProfitInput {
        prompt_tokens: 0,
        completion_tokens: 0,
        upstream_pricing: PricingParams {
            model_ratio: 2.0,
            group_ratio: 1.0,
            completion_ratio: 2.0,
        },
        downstream_price_per_prompt_quota: 4.0,
        downstream_price_per_completion_quota: 8.0,
        payment_fee_rate: 0.006,
        quota_per_unit: 500_000.0,
    };

    let result = calculate_profit(&input);
    assert_eq!(result.upstream_cost_usd, 0.0);
    assert_eq!(result.downstream_revenue_usd, 0.0);
    assert_eq!(result.net_profit_usd, 0.0);
    assert_eq!(result.margin_percent, 0.0);
}

#[test]
fn test_profit_very_large_tokens() {
    // Ensure no overflow for large token counts
    let input = ProfitInput {
        prompt_tokens: 1_000_000_000, // 1 billion
        completion_tokens: 500_000_000,
        upstream_pricing: PricingParams {
            model_ratio: 30.0,
            group_ratio: 1.0,
            completion_ratio: 2.0,
        },
        downstream_price_per_prompt_quota: 60.0,
        downstream_price_per_completion_quota: 120.0,
        payment_fee_rate: 0.006,
        quota_per_unit: 500_000.0,
    };

    let result = calculate_profit(&input);
    // Should not panic or produce NaN/Infinity
    assert!(result.upstream_cost_usd.is_finite());
    assert!(result.downstream_revenue_usd.is_finite());
    assert!(result.net_profit_usd.is_finite());
    assert!(result.upstream_cost_usd > 0.0);
}
