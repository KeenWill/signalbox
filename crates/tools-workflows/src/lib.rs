//! Session workflow declarations and execution over a daemon-owned program port.

use serde::{Deserialize, Serialize};
use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    ToolArgumentValidator, ToolExecutionInvocation, ToolExecutor, ToolExecutorEvidence,
};
use signalbox_domain::{
    NormalizedToolArguments, ToolApprovalPosture, ToolEffectClass, ToolExecutionErrorDetail,
    ToolPermissionDefault, ToolRequest,
};
use signalbox_tool_contract::{ToolContract, compile_contract_definition, decode_canonical_uuid};
use std::{collections::BTreeMap, future::Future};
use uuid::Uuid;

/// Stable model-facing workflow names.
pub const WORKFLOW_TOOL_NAMES: [&str; 6] = [
    "workflow_list",
    "workflow_read",
    "workflow_start",
    "workflow_stop",
    "workflow_replay",
    "workflow_register",
];

/// Operation selected by a compiled declaration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    List,
    Read,
    Start,
    Stop,
    Replay,
    Register,
}

impl Operation {
    /// Returns the model-facing spelling.
    pub const fn name(self) -> &'static str {
        match self {
            Self::List => "workflow_list",
            Self::Read => "workflow_read",
            Self::Start => "workflow_start",
            Self::Stop => "workflow_stop",
            Self::Replay => "workflow_replay",
            Self::Register => "workflow_register",
        }
    }
    /// Resolves a workflow declaration name.
    pub fn from_name(name: &str) -> Option<Self> {
        [
            Self::List,
            Self::Read,
            Self::Start,
            Self::Stop,
            Self::Replay,
            Self::Register,
        ]
        .into_iter()
        .find(|operation| operation.name() == name)
    }
    /// Default posture independently of the dangerous-tool blanket.
    pub const fn default_posture(self) -> ToolApprovalPosture {
        match self {
            Self::List | Self::Read => ToolApprovalPosture::Auto,
            _ => ToolApprovalPosture::Delegated,
        }
    }
    /// Whether the grant names registration targets.
    pub const fn targets_registration(self) -> bool {
        matches!(self, Self::Start | Self::Replay | Self::Register)
    }
}

/// Exact registration-name scope in template configuration.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum RegistrationNames {
    All(String),
    Names(Vec<String>),
}

impl RegistrationNames {
    fn permits(&self, name: &str) -> bool {
        match self {
            Self::All(all) => all == "*",
            Self::Names(names) => names.iter().any(|candidate| candidate == name),
        }
    }
}

/// One template operation's authority and approval selection.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperationPolicy {
    pub enabled: Option<bool>,
    pub names: Option<RegistrationNames>,
    pub posture: Option<ApprovalPosture>,
}

/// Existing approval spellings on the template surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPosture {
    Auto,
    Delegated,
    Human,
}

impl From<ApprovalPosture> for ToolApprovalPosture {
    fn from(value: ApprovalPosture) -> Self {
        match value {
            ApprovalPosture::Auto => Self::Auto,
            ApprovalPosture::Delegated => Self::Delegated,
            ApprovalPosture::Human => Self::Human,
        }
    }
}

/// Reloadable template authority; absent operations refuse execution.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct WorkflowPolicy(pub BTreeMap<Operation, OperationPolicy>);

impl WorkflowPolicy {
    /// Rejects mixed grant shapes and wildcard spellings other than `*`.
    pub fn valid(&self) -> bool {
        self.0.iter().all(|(operation, policy)| {
            if operation.targets_registration() {
                policy.enabled.is_none()
                    && policy.names.as_ref().is_some_and(|names| match names {
                        RegistrationNames::All(all) => all == "*",
                        RegistrationNames::Names(names) => names
                            .iter()
                            .all(|name| !name.is_empty() && !name.contains('\0') && name != "*"),
                    })
            } else {
                policy.names.is_none() && policy.enabled.is_some()
            }
        })
    }
    /// Resolves the ordinary posture frozen when the call is proposed.
    pub fn posture(&self, operation: Operation) -> ToolApprovalPosture {
        self.0
            .get(&operation)
            .and_then(|policy| policy.posture)
            .map(Into::into)
            .unwrap_or_else(|| operation.default_posture())
    }
    /// Checks operation authority and, where required, the resolved registration name.
    pub fn permits(&self, operation: Operation, name: Option<&str>) -> bool {
        self.0.get(&operation).is_some_and(|policy| {
            if operation.targets_registration() {
                name.is_some_and(|name| {
                    policy
                        .names
                        .as_ref()
                        .is_some_and(|names| names.permits(name))
                })
            } else {
                policy.enabled == Some(true)
            }
        })
    }
}

#[derive(Clone, Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListArguments {
    pub after: Option<String>,
}
#[derive(Clone, Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunArguments {
    pub run_id: String,
}
#[derive(Clone, Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartArguments {
    pub name: String,
    pub revision: String,
    pub input: Vec<u8>,
}
#[derive(Clone, Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterArguments {
    pub name: String,
    pub revision: String,
    pub source_path: String,
    pub artifact_path: String,
    pub grants: Vec<ProgramGrant>,
}

/// Capability names requested for a JavaScript registration.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ProgramGrant {
    Time,
    Random,
    Sleep,
    Subscribe,
    Session,
    Judge,
    ExecStage,
    Corpus,
    EvalRecord,
    Blob,
    Register,
    RepoWatch,
}

/// Checked operation arguments; caller identities never enter the schema.
#[derive(Clone, Debug)]
pub enum WorkflowRequest {
    List { after: Option<Uuid> },
    Read { run: Uuid },
    Start(StartArguments),
    Stop { run: Uuid },
    Replay { run: Uuid },
    Register(RegisterArguments),
}

impl WorkflowRequest {
    /// Returns the grant operation.
    pub const fn operation(&self) -> Operation {
        match self {
            Self::List { .. } => Operation::List,
            Self::Read { .. } => Operation::Read,
            Self::Start(_) => Operation::Start,
            Self::Stop { .. } => Operation::Stop,
            Self::Replay { .. } => Operation::Replay,
            Self::Register(_) => Operation::Register,
        }
    }
}

macro_rules! contract {
    ($contract:ident, $args:ty, $name:literal, $description:literal) => {
        struct $contract;
        impl ToolContract for $contract {
            type Arguments = $args;
            const NAME: &'static str = $name;
            const DESCRIPTION: &'static str = $description;
        }
    };
}
contract!(
    ListContract,
    ListArguments,
    "workflow_list",
    "Lists retained workflow runs, including registration, state, times and caller ownership. Continue with next_after until null."
);
contract!(
    ReadContract,
    RunArguments,
    "workflow_read",
    "Reads retained workflow state, registration, journal length, and bounded input/result prefixes with extents."
);
contract!(
    StartContract,
    StartArguments,
    "workflow_start",
    "Starts a registered workflow revision with exact input bytes. Requires a start grant for the registration name."
);
contract!(
    StopContract,
    RunArguments,
    "workflow_stop",
    "Cancels a workflow run, including another session's run when authorized. Returns applied, not_found or already_terminal."
);
contract!(
    ReplayContract,
    RunArguments,
    "workflow_replay",
    "Creates a new workflow run using the original run's registration and complete retained input. Requires a replay grant for that name."
);
contract!(
    RegisterContract,
    RegisterArguments,
    "workflow_register",
    "Registers JavaScript source and emitted artifact from files in this session's workspace. Requires a register grant for the name. Native registration is unavailable."
);

fn decode(operation: Operation, arguments: &str) -> Result<WorkflowRequest, ()> {
    fn json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, ()> {
        serde_json::from_str(text).map_err(|_| ())
    }
    fn identity(text: &str) -> Result<Uuid, ()> {
        decode_canonical_uuid(text).ok_or(())
    }
    fn text(value: &str) -> bool {
        !value.contains('\0')
    }
    match operation {
        Operation::List => {
            let args: ListArguments = json(arguments)?;
            Ok(WorkflowRequest::List {
                after: args.after.as_deref().map(identity).transpose()?,
            })
        }
        Operation::Read | Operation::Stop | Operation::Replay => {
            let args: RunArguments = json(arguments)?;
            let run = identity(&args.run_id)?;
            Ok(match operation {
                Operation::Read => WorkflowRequest::Read { run },
                Operation::Stop => WorkflowRequest::Stop { run },
                _ => WorkflowRequest::Replay { run },
            })
        }
        Operation::Start => {
            let args: StartArguments = json(arguments)?;
            if !text(&args.name) || !text(&args.revision) {
                return Err(());
            }
            Ok(WorkflowRequest::Start(args))
        }
        Operation::Register => {
            let args: RegisterArguments = json(arguments)?;
            if !text(&args.name)
                || !text(&args.revision)
                || !workspace_path(&args.source_path)
                || !workspace_path(&args.artifact_path)
            {
                return Err(());
            }
            Ok(WorkflowRequest::Register(args))
        }
    }
}

fn workspace_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

/// Failure to compile the static tool contract.
#[derive(Debug, signalbox_derive::OperatorError)]
#[error("workflow tool contract construction failed")]
pub struct ConstructionError;

#[derive(Clone)]
struct Validator {
    operation: Operation,
    detail: ToolExecutionErrorDetail,
}
impl ToolArgumentValidator for Validator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode(self.operation, arguments.as_str())
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

/// Compiles all six declarations with the ordinary frozen approval postures.
pub fn catalog() -> Result<CompiledToolCatalog, ConstructionError> {
    let detail = ToolExecutionErrorDetail::try_new("invalid workflow arguments".into())
        .map_err(|_| ConstructionError)?;
    fn definition<C: ToolContract>()
    -> Result<signalbox_application::ToolDefinition, ConstructionError> {
        compile_contract_definition::<C>(
            ToolPermissionDefault::Auto,
            ToolEffectClass::ExternalEffect,
        )
        .map_err(|_| ConstructionError)
    }
    let definitions = [
        definition::<ListContract>()?,
        definition::<ReadContract>()?,
        definition::<StartContract>()?,
        definition::<StopContract>()?,
        definition::<ReplayContract>()?,
        definition::<RegisterContract>()?,
    ];
    let compiled = definitions
        .into_iter()
        .map(|definition| {
            let operation =
                Operation::from_name(definition.name().as_str()).ok_or(ConstructionError)?;
            Ok(CompiledTool::new(
                definition.with_approval_posture(operation.default_posture()),
                Validator {
                    operation,
                    detail: detail.clone(),
                },
            ))
        })
        .collect::<Result<Vec<_>, ConstructionError>>()?;
    CompiledToolCatalog::try_new(compiled).map_err(|_| ConstructionError)
}

/// Daemon adapter for the existing durable program handlers and template grants.
pub trait WorkflowPort: Send {
    type Error: ClassifyOperatorFailure;
    fn execute(
        &mut self,
        caller: &ToolRequest,
        request: WorkflowRequest,
    ) -> impl Future<Output = Result<ToolExecutorEvidence, Self::Error>> + Send;
}

/// Executor matching the workflow declarations.
#[derive(Clone, Debug)]
pub struct WorkflowExecutor<Port>(pub Port);

impl<Port: WorkflowPort> ToolExecutor for WorkflowExecutor<Port> {
    type Error = Port::Error;
    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let request = invocation.request();
        let decoded = Operation::from_name(request.name().as_str())
            .ok_or(())
            .and_then(|operation| decode(operation, request.arguments().as_str()));
        let evidence = match decoded {
            Ok(decoded) => self.0.execute(request, decoded).await?,
            Err(()) => ToolExecutorEvidence::KnownFailed { detail: None },
        };
        Ok(invocation.bind(evidence))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests assert checked fixture construction"
)]
mod tests {
    use super::*;
    use signalbox_application::ToolCatalog;
    #[test]
    fn target_grants_never_authorize_another_name_or_operation() {
        let policy: WorkflowPolicy = serde_json::from_str(
            r#"{"start":{"names":["build"]},"stop":{"enabled":true,"posture":"human"}}"#,
        )
        .unwrap();
        assert!(policy.valid());
        assert!(policy.permits(Operation::Start, Some("build")));
        assert!(!policy.permits(Operation::Start, Some("release")));
        assert!(!policy.permits(Operation::Replay, Some("build")));
        assert_eq!(policy.posture(Operation::Stop), ToolApprovalPosture::Human);
    }
    #[test]
    fn register_rejects_paths_outside_the_workspace() {
        for path in [
            "../source.js",
            "/source.js",
            "a/../source.js",
            "a//source.js",
        ] {
            let args = serde_json::json!({"name":"build","revision":"1","source_path":path,"artifact_path":"out.js","grants":[]});
            assert!(
                decode(Operation::Register, &args.to_string()).is_err(),
                "{path}"
            );
        }
    }
    #[test]
    fn start_retains_every_input_byte_and_rejects_model_supplied_identity() {
        let args = r#"{"name":"build","revision":"1","input":[0,255,10]}"#;
        let WorkflowRequest::Start(start) = decode(Operation::Start, args).unwrap() else {
            panic!("start")
        };
        assert_eq!(start.input, [0, 255, 10]);
        assert!(decode(Operation::Start, r#"{"name":"build","revision":"1","input":[],"run_id":"00000000-0000-0000-0000-000000000000"}"#).is_err());
    }
    #[test]
    fn catalog_defaults_reads_to_automatic_and_mutations_to_the_judge() {
        let catalog = catalog().unwrap();
        for (name, expected) in [
            ("workflow_list", ToolApprovalPosture::Auto),
            ("workflow_read", ToolApprovalPosture::Auto),
            ("workflow_start", ToolApprovalPosture::Delegated),
            ("workflow_stop", ToolApprovalPosture::Delegated),
            ("workflow_replay", ToolApprovalPosture::Delegated),
            ("workflow_register", ToolApprovalPosture::Delegated),
        ] {
            let name = signalbox_domain::ToolName::try_new(name.to_owned()).unwrap();
            assert_eq!(
                catalog.definition(&name).unwrap().approval_posture(),
                Some(expected)
            );
        }
    }
}
