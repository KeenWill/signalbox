//! Typed structured output: the contract and its pure decode.
//!
//! Decoding is a pure function over already-delivered response material.
//! It never performs a model call: when decoding fails, the failure is
//! returned as a typed class and any repair attempt is a new, explicitly
//! authorized operation owned by the caller (docs/spec/runtime-substrate.md).

use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use crate::message::AssistantPart;
use crate::tool::ToolName;

/// A structured-output contract: the caller's demand that the response carry
/// one value satisfying a JSON Schema.
///
/// Adapters realize the contract with provider mechanics (the smoke-critical
/// provider realizes it as a forced tool call); the decode side is provider
/// independent.
#[derive(Debug, Clone, PartialEq)]
pub struct StructuredOutputContract {
    /// The name the value is proposed under on the wire.
    pub name: ToolName,
    /// What the value means, addressed to the model.
    pub description: String,
    /// JSON Schema the value must satisfy.
    pub schema: serde_json::Value,
}

impl StructuredOutputContract {
    /// A contract whose schema is generated from `T`.
    pub fn of_type<T: JsonSchema>(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: ToolName::new(name),
            description: description.into(),
            schema: schemars::schema_for!(T).to_value(),
        }
    }
}

/// Validates a decoded value against caller-owned domain rules, returning
/// structured issues.
///
/// The issue type is the caller's own; this layer transports it without
/// interpretation.
pub trait DomainValidator<T> {
    /// The caller's structured issue type.
    type Issue;

    /// Validates the decoded value, returning every issue found.
    fn validate(&self, value: &T) -> Result<(), Vec<Self::Issue>>;
}

/// A validator imposing no domain constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NoDomainConstraints;

impl<T> DomainValidator<T> for NoDomainConstraints {
    type Issue = std::convert::Infallible;

    fn validate(&self, _value: &T) -> Result<(), Vec<Self::Issue>> {
        Ok(())
    }
}

/// Why response material did not decode into the contracted type.
///
/// The classes stay distinct so the caller can react differently to a
/// response with no structured value, more than one, malformed JSON,
/// well-formed JSON of the wrong shape, and a well-shaped value its domain
/// rejects.
#[derive(Debug, Clone, PartialEq)]
pub enum StructuredDecodeFailure<I> {
    /// The response carries no proposal for the contract.
    NoStructuredValue,
    /// The response carries more than one proposal for the contract, which
    /// promises exactly one value; picking one silently would let provider
    /// part ordering choose the result.
    MultipleStructuredValues {
        /// How many proposals carried the contract's name.
        count: usize,
    },
    /// The proposed value is not syntactically valid JSON.
    JsonSyntax {
        /// The parser's rendered description.
        detail: String,
    },
    /// The proposed value is valid JSON that does not deserialize as the
    /// contracted type.
    SchemaMismatch {
        /// The deserializer's rendered description.
        detail: String,
    },
    /// The value deserialized but the caller's domain validator rejected it.
    DomainInvalid {
        /// The validator's structured issues.
        issues: Vec<I>,
    },
}

/// Decodes the contracted structured value from assistant response parts.
///
/// Finds the proposal carrying the contract's name — exactly one must be
/// present — then decodes and validates it via [`decode_structured_json`].
/// [`crate::ModelOperation::validate`] reserves the contract name from
/// ordinary declared tools before an adapter crosses the request boundary.
pub fn decode_structured<T, V>(
    content: &[AssistantPart],
    contract: &StructuredOutputContract,
    validator: &V,
) -> Result<T, StructuredDecodeFailure<V::Issue>>
where
    T: DeserializeOwned,
    V: DomainValidator<T>,
{
    let mut matching = content.iter().filter_map(|part| match part {
        AssistantPart::ToolCall(proposal) if proposal.name == contract.name => Some(proposal),
        _ => None,
    });
    let Some(first) = matching.next() else {
        return Err(StructuredDecodeFailure::NoStructuredValue);
    };
    let extras = matching.count();
    if extras > 0 {
        return Err(StructuredDecodeFailure::MultipleStructuredValues { count: extras + 1 });
    }
    decode_structured_json(&first.arguments_json, validator)
}

/// Decodes one JSON text into the contracted type, distinguishing syntax,
/// schema, and domain failure classes.
///
/// The schema class is decided by deserialization into `T`: it catches
/// missing fields, wrong types, and every other shape `T` rejects.
/// Schema-annotation constraints that deserialization cannot see (string
/// length bounds, numeric ranges) are the provider's wire contract to
/// honor; a caller that must enforce them locally states them in its
/// [`DomainValidator`], whose issues come back as the domain class.
pub fn decode_structured_json<T, V>(
    json_text: &str,
    validator: &V,
) -> Result<T, StructuredDecodeFailure<V::Issue>>
where
    T: DeserializeOwned,
    V: DomainValidator<T>,
{
    let value: serde_json::Value =
        serde_json::from_str(json_text).map_err(|error| StructuredDecodeFailure::JsonSyntax {
            detail: error.to_string(),
        })?;
    let typed: T =
        serde_json::from_value(value).map_err(|error| StructuredDecodeFailure::SchemaMismatch {
            detail: error.to_string(),
        })?;
    validator
        .validate(&typed)
        .map_err(|issues| StructuredDecodeFailure::DomainInvalid { issues })?;
    Ok(typed)
}

#[cfg(test)]
mod tests {
    use super::{
        DomainValidator, NoDomainConstraints, StructuredDecodeFailure, StructuredOutputContract,
        decode_structured, decode_structured_json,
    };
    use crate::message::AssistantPart;
    use crate::tool::{ToolCallId, ToolCallProposal, ToolName};

    #[derive(Debug, PartialEq, serde::Deserialize, schemars::JsonSchema)]
    struct Verdict {
        approved: bool,
        score: i64,
    }

    /// Rejects any verdict whose score is negative.
    struct NonNegativeScore;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum VerdictIssue {
        NegativeScore,
    }

    impl DomainValidator<Verdict> for NonNegativeScore {
        type Issue = VerdictIssue;

        fn validate(&self, value: &Verdict) -> Result<(), Vec<VerdictIssue>> {
            if value.score < 0 {
                return Err(vec![VerdictIssue::NegativeScore]);
            }
            Ok(())
        }
    }

    fn contract() -> StructuredOutputContract {
        StructuredOutputContract::of_type::<Verdict>("verdict", "The review verdict.")
    }

    fn verdict_proposal(arguments_json: &str) -> AssistantPart {
        AssistantPart::ToolCall(ToolCallProposal {
            id: ToolCallId::new("call_1"),
            name: ToolName::new("verdict"),
            arguments_json: arguments_json.to_string(),
        })
    }

    #[test]
    fn contracted_proposal_decodes_to_the_typed_value() {
        let content = [
            AssistantPart::Text("Reviewing.".to_string()),
            verdict_proposal(r#"{"approved":true,"score":7}"#),
        ];

        let decoded: Verdict = decode_structured(&content, &contract(), &NonNegativeScore)
            .expect("a well-formed contracted proposal decodes");

        assert_eq!(
            decoded,
            Verdict {
                approved: true,
                score: 7
            }
        );
    }

    #[test]
    fn content_without_the_contracted_proposal_is_no_structured_value() {
        let content = [AssistantPart::Text("No value here.".to_string())];

        let failure = decode_structured::<Verdict, _>(&content, &contract(), &NonNegativeScore)
            .expect_err("content without the contracted proposal must not decode");

        assert_eq!(failure, StructuredDecodeFailure::NoStructuredValue);
    }

    #[test]
    fn two_proposals_for_the_contract_are_an_explicit_multiplicity_failure() {
        let content = [
            verdict_proposal(r#"{"approved":true,"score":7}"#),
            verdict_proposal(r#"{"approved":false,"score":1}"#),
        ];

        let failure = decode_structured::<Verdict, _>(&content, &contract(), &NonNegativeScore)
            .expect_err("a contract promising one value must not silently pick among two");

        assert_eq!(
            failure,
            StructuredDecodeFailure::MultipleStructuredValues { count: 2 }
        );
    }

    #[test]
    fn proposal_under_another_name_is_no_structured_value() {
        let content = [AssistantPart::ToolCall(ToolCallProposal {
            id: ToolCallId::new("call_1"),
            name: ToolName::new("other_tool"),
            arguments_json: r#"{"approved":true,"score":7}"#.to_string(),
        })];

        let failure = decode_structured::<Verdict, _>(&content, &contract(), &NonNegativeScore)
            .expect_err("a proposal under another name is not the contracted value");

        assert_eq!(failure, StructuredDecodeFailure::NoStructuredValue);
    }

    #[test]
    fn malformed_json_fails_as_syntax_class() {
        let failure = decode_structured_json::<Verdict, _>("{oops", &NoDomainConstraints)
            .expect_err("malformed JSON must fail as the syntax class");

        assert!(matches!(
            failure,
            StructuredDecodeFailure::JsonSyntax { .. }
        ));
    }

    #[test]
    fn wrong_shape_fails_as_schema_class() {
        let failure =
            decode_structured_json::<Verdict, _>(r#"{"approved":"yes"}"#, &NoDomainConstraints)
                .expect_err("well-formed JSON of the wrong shape must fail as the schema class");

        assert!(matches!(
            failure,
            StructuredDecodeFailure::SchemaMismatch { .. }
        ));
    }

    #[test]
    fn domain_rejection_carries_the_validator_issues() {
        let failure = decode_structured_json::<Verdict, _>(
            r#"{"approved":false,"score":-2}"#,
            &NonNegativeScore,
        )
        .expect_err("a domain-invalid value must fail as the domain class");

        assert_eq!(
            failure,
            StructuredDecodeFailure::DomainInvalid {
                issues: vec![VerdictIssue::NegativeScore]
            }
        );
    }
}
