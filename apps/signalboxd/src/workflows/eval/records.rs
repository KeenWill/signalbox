//! Checked evaluation input and effect payloads shared by native and JavaScript programs.

use serde::{Deserialize, Serialize};
use signalbox_approval_judge_eval::{ApprovalDisposition, ApprovalJudgeCase, live};
use signalbox_domain::{BlobDigest, ProviderReportedTokenUsage};
use signalbox_workflow_runtime::native::NativeProgramError;
use std::collections::BTreeMap;

/// The existing evaluation paid-call safety ceiling.
const MAX_PAID_CALLS: u32 = 1_000;

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EvalManifest {
    pub corpus: String,
    pub format: CorpusFormat,
    pub cases: Vec<u32>,
    pub repeats: u32,
    pub binding: JudgeBinding,
    pub postures: BTreeMap<String, String>,
    pub speculative_tools: Vec<String>,
}

impl EvalManifest {
    pub fn validate(&self) -> Result<(), NativeProgramError> {
        self.corpus
            .parse::<BlobDigest>()
            .map_err(|error| NativeProgramError::new(error.to_string()))?;
        uuid::Uuid::parse_str(&self.binding.selection)
            .map_err(|error| NativeProgramError::new(error.to_string()))?;
        uuid::Uuid::parse_str(&self.binding.target)
            .map_err(|error| NativeProgramError::new(error.to_string()))?;
        if self.cases.is_empty() {
            return Err(NativeProgramError::new(
                "evaluation requires at least one selected case",
            ));
        }
        if self.repeats == 0 || (self.format == CorpusFormat::Offline && self.repeats != 1) {
            return Err(NativeProgramError::new(
                "offline scoring requires one repeat; live repeats must be positive",
            ));
        }
        self.trial_count()?;
        Ok(())
    }

    pub fn trial_count(&self) -> Result<u32, NativeProgramError> {
        u32::try_from(self.cases.len())
            .ok()
            .and_then(|count| count.checked_mul(self.repeats))
            .filter(|count| *count <= MAX_PAID_CALLS)
            .ok_or_else(|| {
                NativeProgramError::new("evaluation exceeds the 1000-call safety ceiling")
            })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CorpusFormat {
    Offline,
    Live,
}

/// Non-secret host-resolved binding and operation contract pinned in run input.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JudgeBinding {
    pub selection: String,
    pub target: String,
    pub credential_reference: String,
    pub provider_model: String,
    pub contract_digest: String,
    pub cache_accounting: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(
    tag = "format",
    content = "case",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Case {
    Offline(ApprovalJudgeCase),
    Live(#[serde(with = "live_case")] live::CorpusCase),
}

mod live_case {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        case: &live::CorpusCase,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut value = serde_json::to_value(case).map_err(serde::ser::Error::custom)?;
        if let Some(fence) = &case.dispatch {
            value["dispatch"]["pull_request"] = fence.pull_request.to_string().into();
        }
        value.serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<live::CorpusCase, D::Error> {
        let mut value = serde_json::Value::deserialize(deserializer)?;
        if let Some(fence) = value.get_mut("dispatch").filter(|value| !value.is_null()) {
            let text = fence
                .get("pull_request")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| serde::de::Error::custom("expected decimal pull request"))?;
            let number: u64 = text.parse().map_err(serde::de::Error::custom)?;
            if number.to_string() != text {
                return Err(serde::de::Error::custom("noncanonical pull request"));
            }
            fence["pull_request"] = number.into();
        }
        serde_json::from_value(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CorpusAnswer {
    pub cases: Vec<Case>,
    pub corpus_digest: String,
    pub rendered_digest: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialRequest {
    pub trial: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlobReadRequest {
    pub digest: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlobAnswer {
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum JudgeAnswer {
    Verdict {
        call: String,
        request_digest: String,
        binding: JudgeBinding,
        actual: ApprovalDisposition,
        rationale: String,
        provider_reported_model: Option<String>,
        usage: Usage,
    },
    Failed {
        call: Option<String>,
        request_digest: String,
        binding: JudgeBinding,
        cause: String,
        provider_reported_model: Option<String>,
        usage: Usage,
    },
    Ambiguous,
}

/// Decimal strings preserve full-width provider counts through JavaScript.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    #[serde(with = "optional_count")]
    pub input_tokens: Option<u64>,
    #[serde(with = "optional_count")]
    pub output_tokens: Option<u64>,
    #[serde(with = "optional_count")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(with = "optional_count")]
    pub cache_read_input_tokens: Option<u64>,
}

mod optional_count {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        value: &Option<u64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.map(|count| count.to_string()).serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|value| {
                let count: u64 = value.parse().map_err(serde::de::Error::custom)?;
                if count.to_string() != value {
                    return Err(serde::de::Error::custom("noncanonical token count"));
                }
                Ok(count)
            })
            .transpose()
    }
}

impl Usage {
    pub(super) fn domain(&self) -> ProviderReportedTokenUsage {
        ProviderReportedTokenUsage::unreported()
            .with_input_tokens(self.input_tokens)
            .with_output_tokens(self.output_tokens)
            .with_cache_creation_input_tokens(self.cache_creation_input_tokens)
            .with_cache_read_input_tokens(self.cache_read_input_tokens)
    }
}
