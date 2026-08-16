//! Offline replay surface for the exact approval-judge prompt path.
//!
//! The `approval-judge-eval` binary replays labeled cases through the same
//! system prompt, payload rendering, structured-output contract, and provider
//! adapter the daemon uses for live delegated approvals, so a measured verdict
//! reflects the deployed judge rather than a reimplementation. Nothing here is
//! reachable from daemon execution; the daemon path keeps its own wiring.

use std::{error::Error, fmt};

use signalbox_application::ApprovalJudgeAuthorization;
use signalbox_domain::{
    DelegateApprovalRecommendation, DirectModelSelection, GoalStatement, ModelCallId,
    NormalizedToolArguments, ResolvedProviderTarget, SessionId, SessionSystemPrompt,
    SessionTemplateName, ToolApprovalPosture, ToolName, ToolRequest, ToolRequestId,
    ToolRequestOrdinal, ToolRequestReconstitutionInput, TurnId,
};
use signalbox_model_provider_runtime::{ApprovalJudgeModel, ApprovalJudgeModelRequest};
use signalbox_model_runtime::TokenUsage;
use signalbox_persistence::approval_judge::SessionAuthorityContext;

use crate::{APPROVAL_JUDGE_SYSTEM_PROMPT, JudgeRequestFields, render_judge_request_payload};

/// One labeled request replayed against the deployed judge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalJudgeEvalCase {
    /// Stable case identity; also seeds the rendered request identifier.
    pub name: String,
    /// Exact tool name the judged request proposes.
    pub tool: String,
    /// Exact argument text as a producing model would propose it.
    pub arguments: String,
    /// Session goal carried in the untrusted context block, when present.
    pub goal: Option<String>,
    /// Session template name carried in the untrusted context block.
    pub template: Option<String>,
    /// Frozen system prompt carried in the untrusted context block.
    pub system_prompt: Option<String>,
}

/// The provider binding an eval run resolves once from configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalJudgeEvalBinding {
    /// Direct judge selection frozen for every replayed call.
    pub selection: DirectModelSelection,
    /// Exact resolved provider target for the selection.
    pub target: ResolvedProviderTarget,
    /// Non-secret credential reference recorded in call evidence.
    pub credential_reference: String,
}

/// One verdict returned by a single replayed judge call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalJudgeEvalVerdict {
    /// Closed recommendation the judge emitted.
    pub recommendation: DelegateApprovalRecommendation,
    /// Exact rationale emitted with the recommendation.
    pub rationale: String,
    /// Concrete model identity reported by the provider, when observable.
    pub provider_reported_model: Option<String>,
    /// Provider-reported usage, so callers can apply the same configured
    /// usage limits the daemon applies before accepting a verdict.
    pub usage: TokenUsage,
}

/// A case carried text one of the admitted domain values rejects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalJudgeEvalCaseError {
    field: &'static str,
    admission_failure: String,
}

impl ApprovalJudgeEvalCaseError {
    /// Names the rejected case field.
    #[must_use]
    pub const fn field(&self) -> &'static str {
        self.field
    }

    /// Renders the underlying domain admission failure.
    #[must_use]
    pub fn admission_failure(&self) -> &str {
        &self.admission_failure
    }
}

impl fmt::Display for ApprovalJudgeEvalCaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "eval case field {} is not admitted: {}",
            self.field, self.admission_failure
        )
    }
}

impl Error for ApprovalJudgeEvalCaseError {}

/// Returns the exact system prompt the daemon sends with every judge call.
#[must_use]
pub fn judge_system_prompt() -> &'static str {
    APPROVAL_JUDGE_SYSTEM_PROMPT
}

/// Renders one case exactly as the daemon renders a parked request.
///
/// The request identity derives deterministically from the case name, so a
/// rerun of the same corpus replays byte-identical payloads.
pub fn render_eval_case(
    case: &ApprovalJudgeEvalCase,
) -> Result<String, ApprovalJudgeEvalCaseError> {
    let request = eval_tool_request(case)?;
    let context = eval_session_context(case)?;
    let request_id = request.id().into_uuid().to_string();
    Ok(render_judge_request_payload(
        &JudgeRequestFields {
            request_id: &request_id,
            tool: request.name().as_str(),
            arguments_kind: request.arguments().kind(),
            arguments: request.arguments().as_str(),
        },
        &context,
    ))
}

/// Executes one replayed judge call through a prepared provider model.
///
/// Every replay draws a fresh call identity, so repeated executions of one
/// case measure verdict stability rather than provider-side deduplication.
pub async fn judge_eval_case(
    model: &dyn ApprovalJudgeModel,
    binding: &ApprovalJudgeEvalBinding,
    case: &ApprovalJudgeEvalCase,
) -> Result<ApprovalJudgeEvalVerdict, Box<dyn Error + Send + Sync>> {
    let request = eval_tool_request(case)?;
    let rendered_request = render_eval_case(case)?;
    let call = ModelCallId::from_uuid(uuid::Uuid::now_v7());
    let model_request = ApprovalJudgeModelRequest {
        request: request.clone(),
        call,
        selection: binding.selection,
        target: binding.target,
        credential_reference: binding.credential_reference.clone(),
        system_prompt: String::from(APPROVAL_JUDGE_SYSTEM_PROMPT),
        rendered_request,
    };
    let prepared = model.prepare(model_request).await?;
    let result = prepared
        .execute(EvalAuthorization {
            request,
            call,
            selection: binding.selection,
            target: binding.target,
            credential_reference: binding.credential_reference.clone(),
        })
        .await?;
    Ok(ApprovalJudgeEvalVerdict {
        recommendation: result.recommendation,
        rationale: result.rationale.into_string(),
        provider_reported_model: result.reported_model.map(|model| model.as_str().to_owned()),
        usage: result.usage,
    })
}

/// Distinguishes the synthetic turn identity from the request identity
/// derived from the same case name.
const TURN_IDENTITY_SALT: u128 = 1;
/// Distinguishes the synthetic producing-call identity from both.
const PRODUCING_CALL_IDENTITY_SALT: u128 = 2;

fn eval_tool_request(
    case: &ApprovalJudgeEvalCase,
) -> Result<ToolRequest, ApprovalJudgeEvalCaseError> {
    let identity = fnv1a_128(case.name.as_bytes());
    let name =
        ToolName::try_new(case.tool.clone()).map_err(|error| ApprovalJudgeEvalCaseError {
            field: "tool",
            admission_failure: format!("{error:?}"),
        })?;
    let arguments = NormalizedToolArguments::try_from_provider_text(case.arguments.clone())
        .map_err(|error| ApprovalJudgeEvalCaseError {
            field: "arguments",
            admission_failure: format!("{error:?}"),
        })?;
    Ok(ToolRequestReconstitutionInput::new(
        ToolRequestId::from_uuid(production_shaped_uuid(identity)),
        SessionId::from_uuid(uuid::Uuid::from_u128(fnv1a_128(b"approval-judge-eval"))),
        TurnId::from_uuid(uuid::Uuid::from_u128(identity ^ TURN_IDENTITY_SALT)),
        ModelCallId::from_uuid(uuid::Uuid::from_u128(
            identity ^ PRODUCING_CALL_IDENTITY_SALT,
        )),
        ToolRequestOrdinal::from_u32(0),
        name,
        arguments,
    )
    .with_approval_posture(ToolApprovalPosture::Delegated)
    .into_request())
}

/// Retains deterministic hash material while matching the RFC variant and
/// version bits of the UUIDv7 request identities emitted by production.
fn production_shaped_uuid(identity: u128) -> uuid::Uuid {
    let mut bytes = identity.to_be_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes)
}

fn eval_session_context(
    case: &ApprovalJudgeEvalCase,
) -> Result<SessionAuthorityContext, ApprovalJudgeEvalCaseError> {
    let goal = case
        .goal
        .clone()
        .map(|value| {
            GoalStatement::try_new(value).map_err(|error| ApprovalJudgeEvalCaseError {
                field: "goal",
                admission_failure: error.to_string(),
            })
        })
        .transpose()?;
    let template = case
        .template
        .clone()
        .map(|value| {
            SessionTemplateName::try_new(value).map_err(|error| ApprovalJudgeEvalCaseError {
                field: "template",
                admission_failure: error.to_string(),
            })
        })
        .transpose()?;
    let system_prompt = case
        .system_prompt
        .clone()
        .map(|value| {
            SessionSystemPrompt::try_new(value).map_err(|error| ApprovalJudgeEvalCaseError {
                field: "system_prompt",
                admission_failure: error.to_string(),
            })
        })
        .transpose()?;
    Ok(SessionAuthorityContext::new(goal, template, system_prompt))
}

/// Stable 128-bit FNV-1a so case identities survive toolchain upgrades.
fn fnv1a_128(bytes: &[u8]) -> u128 {
    const OFFSET_BASIS: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

struct EvalAuthorization {
    request: ToolRequest,
    call: ModelCallId,
    selection: DirectModelSelection,
    target: ResolvedProviderTarget,
    credential_reference: String,
}

impl ApprovalJudgeAuthorization for EvalAuthorization {
    fn request(&self) -> &ToolRequest {
        &self.request
    }

    fn call(&self) -> ModelCallId {
        self.call
    }

    fn selection(&self) -> DirectModelSelection {
        self.selection
    }

    fn target(&self) -> ResolvedProviderTarget {
        self.target
    }

    fn credential_reference(&self) -> &str {
        &self.credential_reference
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case() -> ApprovalJudgeEvalCase {
        ApprovalJudgeEvalCase {
            name: String::from("fixture-push-own-branch"),
            tool: String::from("unsandboxed_exec"),
            arguments: String::from(r#"{"program":"git","arguments":["push"]}"#),
            goal: Some(String::from("Fix the reviewer finding on the head branch.")),
            template: Some(String::from("review-response")),
            system_prompt: Some(String::from("Push only the head branch.")),
        }
    }

    #[test]
    fn rendered_case_is_deterministic() {
        let first = render_eval_case(&case()).expect("fixture case renders");
        let second = render_eval_case(&case()).expect("fixture case renders");
        assert_eq!(first, second);
    }

    #[test]
    fn rendered_request_identity_has_production_uuid_shape() {
        let request = eval_tool_request(&case()).expect("fixture case is admitted");
        let identity = request.id().into_uuid();

        assert_eq!(identity.get_version_num(), 7);
        assert_eq!(identity.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn rendered_case_quotes_session_context_beside_the_request() {
        let case = case();
        let rendered = render_eval_case(&case).expect("fixture case renders");
        assert!(rendered.contains("session_context"));
        assert!(rendered.contains(&case.tool));
        assert!(rendered.contains(case.template.as_deref().expect("fixture sets template")));
    }

    #[test]
    fn rendered_case_reports_the_absent_goal_block_explicitly() {
        let mut absent = case();
        absent.goal = None;
        let rendered = render_eval_case(&absent).expect("fixture case renders");
        let payload: serde_json::Value =
            serde_json::from_str(&rendered).expect("rendered payload is JSON");
        let context = payload["session_context"]
            .as_str()
            .expect("session_context renders as text");
        assert!(
            context.contains("-----BEGIN UNTRUSTED SESSION CONTEXT: session_goal-----\n(absent)\n")
        );
        assert!(
            context.contains(
                absent
                    .system_prompt
                    .as_deref()
                    .expect("fixture sets system prompt")
            )
        );
    }

    #[test]
    fn inadmissible_case_fields_are_named() {
        let mut broken = case();
        broken.tool = String::new();
        let error = render_eval_case(&broken).expect_err("empty tool name is rejected");
        assert_eq!(error.field(), "tool");
    }
}
