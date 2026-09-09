//! Shared test fixtures.

use super::*;
use signalbox_persistence::test_support::postgres::TestDatabase;

pub(crate) const RESPONSE_ALLOWANCE: Duration = Duration::from_secs(30);
pub(crate) const RUNTIME_SETTLE_ALLOWANCE: Duration = Duration::from_secs(60);

pub(crate) const MAX_SUBMITTED_INPUT_BYTES: usize = 1024 * 1024;
pub(crate) const MODEL_CONFIGURATION: &str = r#"
version = 1

[[credential_profiles]]
name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/anthropic-primary"

[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]


[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "anthropic-main"

[compaction]
prompt = "Summarize the prior conversation faithfully for continuation."

[[models]]
selection_id = "00000000-0000-0000-0000-000000000001"
target_id = "00000000-0000-0000-0000-000000000003"
model_family = "anthropic"
provider_model = "fixture-model"
max_output_tokens = 256
context_window_tokens = 200000
reasoning_levels = ["low"]

[[models]]
selection_id = "00000000-0000-0000-0000-000000000004"
target_id = "00000000-0000-0000-0000-000000000005"
model_family = "anthropic"
provider_model = "fixture-model-next"
max_output_tokens = 256
context_window_tokens = 200000

[[aliases]]
alias_id = "00000000-0000-0000-0000-000000000002"
selection_id = "00000000-0000-0000-0000-000000000001"

[[aliases]]
alias_id = "7fde05bc-b4c3-44f7-8a87-748814c80191"
selection_id = "00000000-0000-0000-0000-000000000001"

[[aliases]]
alias_id = "540ce009-c2ec-4a04-b823-c411ea189778"
selection_id = "00000000-0000-0000-0000-000000000001"
"#;

pub(crate) fn reported_usage_preflight_configuration_text() -> String {
    MODEL_CONFIGURATION
        // These fixtures exercise reported-usage preflight through an adapter
        // without prospective token counting.
        .replace("adapter = \"anthropic\"", "adapter = \"openai\"")
        .replace("model_family = \"anthropic\"", "model_family = \"openai\"")
        .replace("max_output_tokens = 256", "max_output_tokens = 16")
        .replace(
            "context_window_tokens = 200000",
            "context_window_tokens = 4096",
        )
}

pub(crate) fn reported_usage_preflight_configuration()
-> Result<HubModelConfiguration, Box<dyn Error>> {
    Ok(support::parse_model_configuration(
        &reported_usage_preflight_configuration_text(),
    )?)
}

pub(crate) fn session_template_configuration(
    models: &HubModelConfiguration,
) -> Result<SessionTemplateConfiguration, Box<dyn Error>> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/session-templates.example.toml");
    Ok(SessionTemplateConfiguration::read(&path, || None, models)?)
}

pub(crate) async fn postgres() -> Result<(TestDatabase, PgPool), Box<dyn Error>> {
    let (database, pool, _) =
        signalbox_persistence::test_support::postgres::migrated_postgres(8).await?;
    Ok((database, pool))
}

pub(crate) struct SocketDirectory {
    pub(crate) directory: PathBuf,
    pub(crate) socket: PathBuf,
}

impl SocketDirectory {
    pub(crate) fn create() -> Result<Self, Box<dyn Error>> {
        let directory = PathBuf::from("/tmp").join(format!("signalbox-process-{}", Uuid::now_v7()));
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let socket = directory.join("hub.sock");
        Ok(Self { directory, socket })
    }

    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    pub(crate) fn cleanup(self) -> Result<(), Box<dyn Error>> {
        let mut lock = self.socket.into_os_string();
        lock.push(".lock");
        match fs::remove_file(PathBuf::from(lock)) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        fs::remove_dir(self.directory)?;
        Ok(())
    }
}

pub(crate) struct Connection {
    pub(crate) reader: BufReader<OwnedReadHalf>,
    pub(crate) writer: OwnedWriteHalf,
}

impl Connection {
    pub(crate) async fn connect(path: &Path) -> Result<Self, Box<dyn Error>> {
        let stream = UnixStream::connect(path).await?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
        })
    }

    pub(crate) async fn request(
        &mut self,
        request_id: u64,
        request: ClientRequest,
    ) -> Result<(), Box<dyn Error>> {
        self.request_version(ProtocolVersion::One, request_id, request)
            .await
    }

    pub(crate) async fn request_version(
        &mut self,
        version: ProtocolVersion,
        request_id: u64,
        request: ClientRequest,
    ) -> Result<(), Box<dyn Error>> {
        let frame =
            ClientFrame::try_new_for_version(version, RequestId::try_new(request_id)?, request)?;
        self.writer.write_all(&encode_client_line(&frame)?).await?;
        Ok(())
    }

    pub(crate) async fn raw_request(&mut self, frame: &str) -> Result<(), Box<dyn Error>> {
        self.writer.write_all(frame.as_bytes()).await?;
        Ok(())
    }

    pub(crate) async fn response(&mut self) -> Result<ServerFrame, Box<dyn Error>> {
        let mut line = Vec::new();
        if self.reader.read_until(b'\n', &mut line).await? == 0 {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "the process server closed before its next frame",
            )
            .into());
        }
        Ok(decode_server_line(&line)?)
    }
}

pub(crate) fn command() -> Result<CommandId, Box<dyn Error>> {
    Ok(CommandId::try_from_uuid(Uuid::now_v7())?)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SessionCreatedFacts {
    pub(crate) session_id: CanonicalUuid,
    pub(crate) model_settings: ModelSettingsSnapshot,
}

#[track_caller]
pub(crate) fn session_created_facts(message: &ServerMessage) -> SessionCreatedFacts {
    match message {
        ServerMessage::SessionCreated {
            session_id,
            model_settings,
        } => SessionCreatedFacts {
            session_id: *session_id,
            model_settings: *model_settings,
        },
        message => panic!("fixture expected session-created receipt, got {message:?}"),
    }
}

pub(crate) fn primary_direct_selection_id() -> CanonicalUuid {
    CanonicalUuid::from_uuid(Uuid::from_u128(1))
}

pub(crate) fn next_direct_selection_id() -> CanonicalUuid {
    CanonicalUuid::from_uuid(Uuid::from_u128(4))
}

pub(crate) fn low_reasoning_override() -> ModelSettingsOverlay {
    ModelSettingsOverlay {
        reasoning_level: SettingOverlay::Value(ReasoningLevel::Low),
        fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
        service_tier: SettingOverlay::Inherit,
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReconciliationWitness {
    pub(crate) completed_cycles: Arc<AtomicUsize>,
    pub(crate) cycle: Arc<Mutex<ReconciliationCycle>>,
}

pub(crate) struct WitnessedEligibilitySweep<Sweep> {
    pub(crate) inner: Option<Sweep>,
    pub(crate) witness: ReconciliationWitness,
}

impl<Sweep> WitnessedEligibilitySweep<Sweep> {
    pub(crate) fn new(inner: Sweep, witness: ReconciliationWitness) -> Self {
        Self {
            inner: Some(inner),
            witness,
        }
    }
}

impl<Sweep> EligibilitySweep for WitnessedEligibilitySweep<Sweep>
where
    Sweep: EligibilitySweep + Send,
{
    type Error = Sweep::Error;

    fn find_sessions(
        &mut self,
    ) -> impl Future<Output = Result<EligibilitySweepBatch, Self::Error>> + Send {
        let witness = self.witness.clone();
        async move {
            let Some(inner) = self.inner.as_mut() else {
                return pending().await;
            };
            let batch = inner.find_sessions().await?;
            let (sessions, continuation) = batch.clone().into_parts();
            witness.record_batch(&sessions, continuation);
            Ok(batch)
        }
    }
}

pub(crate) type RuntimeEligibilitySweep = WitnessedEligibilitySweep<PostgresEligibilitySweep>;

enum WorkflowFixtureMode {
    Disabled,
    Running,
    Stopped,
}

pub(crate) struct RunningRuntime {
    workflow_task: Option<JoinHandle<Result<(), signalboxd::workflows::WorkflowRuntimeError>>>,
    pub(crate) container: TestDatabase,
    pub(crate) pool: PgPool,
    pub(crate) socket_directory: SocketDirectory,
    pub(crate) shutdown: watch::Sender<bool>,
    pub(crate) runtime_task: Option<JoinHandle<Result<(), ProcessRuntimeError>>>,
    pub(crate) eligibility_nudge: InProcessEligibilityNudge,
    pub(crate) work_source: Option<InProcessEligibilityWorkSource<RuntimeEligibilitySweep>>,
    pub(crate) reconciliation_witness: ReconciliationWitness,
    pub(crate) provider_text_deltas: ProcessProviderTextDeltaSink,
    pub(crate) blob_store_registry: Option<Arc<BlobStoreRegistry>>,
    pub(crate) blob_storage_root: Option<BlobStorageFixture>,
}

#[derive(Clone, Copy)]
pub(crate) enum BlobStorageFixtureMode {
    Disabled,
    Enabled,
}

impl RunningRuntime {
    pub(crate) async fn start() -> Result<Self, Box<dyn Error>> {
        Self::start_with_optional_compaction(None).await
    }

    pub(crate) async fn start_with_compaction(
        model: ScriptedModel<ModelCallId>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::start_with_optional_compaction(Some(model)).await
    }

    pub(crate) async fn start_with_optional_compaction(
        compaction_model: Option<ScriptedModel<ModelCallId>>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::start_with_options(
            compaction_model,
            BlobStorageFixtureMode::Disabled,
            None,
            None,
        )
        .await
    }

    pub(crate) async fn start_with_blob_storage() -> Result<Self, Box<dyn Error>> {
        Self::start_with_options(None, BlobStorageFixtureMode::Enabled, None, None).await
    }

    pub(crate) async fn start_with_model_configuration(
        configuration: &str,
    ) -> Result<Self, Box<dyn Error>> {
        Self::start_with_options(
            None,
            BlobStorageFixtureMode::Disabled,
            Some(configuration),
            None,
        )
        .await
    }

    pub(crate) async fn start_with_options(
        compaction_model: Option<ScriptedModel<ModelCallId>>,
        blob_storage: BlobStorageFixtureMode,
        configuration_override: Option<&str>,
        nudge_capacity: Option<std::num::NonZeroUsize>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::start_with_workflow_options(
            compaction_model,
            blob_storage,
            configuration_override,
            nudge_capacity,
            WorkflowFixtureMode::Disabled,
        )
        .await
    }

    pub(crate) async fn start_programs() -> Result<Self, Box<dyn Error>> {
        Self::start_with_workflow_options(
            None,
            BlobStorageFixtureMode::Disabled,
            None,
            None,
            WorkflowFixtureMode::Running,
        )
        .await
    }

    pub(crate) async fn start_stopped_programs() -> Result<Self, Box<dyn Error>> {
        Self::start_with_workflow_options(
            None,
            BlobStorageFixtureMode::Disabled,
            None,
            None,
            WorkflowFixtureMode::Stopped,
        )
        .await
    }

    async fn start_with_workflow_options(
        compaction_model: Option<ScriptedModel<ModelCallId>>,
        blob_storage: BlobStorageFixtureMode,
        configuration_override: Option<&str>,
        nudge_capacity: Option<std::num::NonZeroUsize>,
        programs: WorkflowFixtureMode,
    ) -> Result<Self, Box<dyn Error>> {
        let (container, pool) = postgres().await?;
        let socket_directory = SocketDirectory::create()?;
        let listener = LocalProcessListener::bind(socket_directory.socket())?;
        let reconciliation_witness = ReconciliationWitness::new();
        let mut sweep = WitnessedEligibilitySweep::new(
            PostgresEligibilitySweep::new(pool.clone()),
            reconciliation_witness.clone(),
        );
        let (eligibility_nudge, work_source) = match nudge_capacity {
            Some(capacity) => {
                sweep.inner = None;
                InProcessEligibilityWorkSource::with_options(sweep, None, Some(capacity))
            }
            None => InProcessEligibilityWorkSource::new(sweep),
        };
        let blob_storage_root = match blob_storage {
            BlobStorageFixtureMode::Disabled => None,
            BlobStorageFixtureMode::Enabled => Some(BlobStorageFixture::create()?),
        };
        let configuration = configuration_override.map_or_else(
            || {
                blob_storage_root.as_ref().map_or_else(
                    || String::from(MODEL_CONFIGURATION),
                    BlobStorageFixture::model_configuration,
                )
            },
            String::from,
        );
        let model_configuration = support::parse_model_configuration(&configuration)?;
        let blob_store_registry = match blob_storage {
            BlobStorageFixtureMode::Disabled => None,
            BlobStorageFixtureMode::Enabled => {
                BlobStoreRegistry::initialize(model_configuration.blob_storage(), pool.clone())
                    .await?
                    .map(Arc::new)
            }
        };
        let runtime_models = model_configuration.runtime_model_catalog();
        let template_configuration = session_template_configuration(&model_configuration)?;
        let mut runtime = ProcessRuntime::new_with_templates(
            listener,
            pool.clone(),
            eligibility_nudge.clone(),
            InProcessToolDispatchGate::default(),
            model_configuration,
            template_configuration,
        );
        if let Some(compaction_model) = compaction_model {
            runtime = runtime.with_context_compaction_model(RuntimeContextCompactionModel::new(
                compaction_model,
                runtime_models,
            ));
        }
        if let Some(registry) = blob_store_registry.as_ref() {
            runtime = runtime.with_blob_store_registry(Arc::clone(registry));
        }
        let provider_text_deltas = runtime.provider_text_delta_sink();
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let workflow_task = match programs {
            WorkflowFixtureMode::Disabled => None,
            WorkflowFixtureMode::Running | WorkflowFixtureMode::Stopped => {
                let (service, workflows) =
                    signalboxd::workflows::WorkflowRuntime::new(pool.clone())?;
                runtime = runtime.with_workflows(service);
                match programs {
                    WorkflowFixtureMode::Running => {
                        let mut stopped = shutdown_receiver.clone();
                        Some(tokio::spawn(workflows.run(async move {
                            let _ = stopped.changed().await;
                        })))
                    }
                    _ => {
                        drop(workflows);
                        None
                    }
                }
            }
        };
        let runtime_task = tokio::spawn(runtime.run(shutdown_receiver));
        Ok(Self {
            workflow_task,
            container,
            pool,
            socket_directory,
            shutdown,
            runtime_task: Some(runtime_task),
            eligibility_nudge,
            work_source: Some(work_source),
            reconciliation_witness,
            provider_text_deltas,
            blob_store_registry,
            blob_storage_root,
        })
    }

    pub(crate) fn socket(&self) -> &Path {
        self.socket_directory.socket()
    }

    pub(crate) async fn restart(&mut self) -> Result<usize, Box<dyn Error>> {
        self.restart_with_model_configuration(MODEL_CONFIGURATION)
            .await
    }

    pub(crate) async fn restart_with_model_configuration(
        &mut self,
        configuration: &str,
    ) -> Result<usize, Box<dyn Error>> {
        let model_configuration = support::parse_model_configuration(configuration)?;
        let template_configuration = session_template_configuration(&model_configuration)?;
        self.restart_with_templates(configuration, template_configuration)
            .await
    }

    /// Restarts over the same database with every session template removed
    /// from configuration, as a template rename or deletion would leave it.
    pub(crate) async fn restart_without_templates(&mut self) -> Result<usize, Box<dyn Error>> {
        self.restart_with_templates(MODEL_CONFIGURATION, SessionTemplateConfiguration::default())
            .await
    }

    pub(crate) async fn restart_with_templates(
        &mut self,
        configuration: &str,
        template_configuration: SessionTemplateConfiguration,
    ) -> Result<usize, Box<dyn Error>> {
        self.shutdown.send_replace(true);
        let runtime_task = self
            .runtime_task
            .as_mut()
            .expect("a running runtime has an installed task");
        timeout(RUNTIME_SETTLE_ALLOWANCE, runtime_task).await???;
        self.runtime_task = None;
        self.restart_after_stop(configuration, template_configuration)
            .await
    }

    pub(crate) async fn restart_after_stop(
        &mut self,
        configuration: &str,
        template_configuration: SessionTemplateConfiguration,
    ) -> Result<usize, Box<dyn Error>> {
        let mut scan = StartupScanService::new(
            UuidV7StartupScanIdGenerator,
            PostgresStartupScanRepository::new(self.pool.clone()),
        );
        let recovered_turn_count = scan.execute().await?.recovered_turn_count();

        let listener = LocalProcessListener::bind(self.socket())?;
        let reconciliation_witness = ReconciliationWitness::new();
        let sweep = WitnessedEligibilitySweep::new(
            PostgresEligibilitySweep::new(self.pool.clone()),
            reconciliation_witness.clone(),
        );
        let (eligibility_nudge, work_source) = InProcessEligibilityWorkSource::new(sweep);
        let model_configuration = support::parse_model_configuration(configuration)?;
        let mut runtime = ProcessRuntime::new_with_templates(
            listener,
            self.pool.clone(),
            eligibility_nudge.clone(),
            InProcessToolDispatchGate::default(),
            model_configuration,
            template_configuration,
        );
        if let Some(registry) = self.blob_store_registry.as_ref() {
            runtime = runtime.with_blob_store_registry(Arc::clone(registry));
        }
        let provider_text_deltas = runtime.provider_text_delta_sink();
        let (shutdown, shutdown_receiver) = watch::channel(false);
        self.shutdown = shutdown;
        self.runtime_task = Some(tokio::spawn(runtime.run(shutdown_receiver)));
        self.eligibility_nudge = eligibility_nudge;
        self.work_source = Some(work_source);
        self.reconciliation_witness = reconciliation_witness;
        self.provider_text_deltas = provider_text_deltas;
        Ok(recovered_turn_count)
    }

    /// Simulates the uncatchable process death used by the fleet soak. The
    /// replacement opens the same socket and database only after the killed
    /// task has stopped, so no graceful runtime shutdown can repair its work.
    pub(crate) async fn kill_and_restart(&mut self) -> Result<usize, Box<dyn Error>> {
        let runtime_task = self
            .runtime_task
            .take()
            .expect("a running runtime has an installed task");
        runtime_task.abort();
        let killed = runtime_task.await;
        let killed = killed.expect_err("the killed runtime task must not return normally");
        assert!(
            killed.is_cancelled(),
            "the runtime task must stop by cancellation, got {killed}"
        );

        let model_configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
        let template_configuration = session_template_configuration(&model_configuration)?;
        self.restart_after_stop(MODEL_CONFIGURATION, template_configuration)
            .await
    }

    pub(crate) fn take_work_source(
        &mut self,
    ) -> InProcessEligibilityWorkSource<RuntimeEligibilitySweep> {
        self.work_source
            .take()
            .expect("the streaming fixture takes the work source once")
    }

    pub(crate) fn reconciliation_witness(&self) -> ReconciliationWitness {
        self.reconciliation_witness.clone()
    }

    pub(crate) fn provider_text_delta_sink(&self) -> ProcessProviderTextDeltaSink {
        self.provider_text_deltas.clone()
    }

    pub(crate) fn blob_store_registry(&self) -> Arc<BlobStoreRegistry> {
        Arc::clone(
            self.blob_store_registry
                .as_ref()
                .expect("the fixture enables blob storage"),
        )
    }

    pub(crate) async fn stop(mut self) -> Result<(), Box<dyn Error>> {
        if let Some(runtime_task) = self.runtime_task.take() {
            self.shutdown.send(true)?;
            timeout(RUNTIME_SETTLE_ALLOWANCE, runtime_task).await???;
        }
        if let Some(task) = self.workflow_task.take() {
            timeout(RUNTIME_SETTLE_ALLOWANCE, task).await???;
        }
        self.pool.close().await;
        self.socket_directory.cleanup()?;
        drop(self.blob_storage_root);
        drop(self.container);
        Ok(())
    }
}

pub(crate) struct BlobStorageFixture {
    pub(crate) _root: TempDir,
    pub(crate) staging: PathBuf,
    pub(crate) store: PathBuf,
}

impl BlobStorageFixture {
    pub(crate) fn create() -> Result<Self, io::Error> {
        let root = TempDir::new()?;
        let staging = root.path().join("staging");
        let store = root.path().join("primary");
        fs::create_dir(&staging)?;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
        fs::create_dir(&store)?;
        fs::set_permissions(&store, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            _root: root,
            staging,
            store,
        })
    }

    pub(crate) fn model_configuration(&self) -> String {
        format!(
            r#"{MODEL_CONFIGURATION}
[blob_storage]
version = 1
staging_directory = "{}"
max_blob_bytes = 21474836480

[[blob_storage.stores]]
name = "primary"
namespace_id = "5a100001-0000-4000-8000-000000000001"
kind = "filesystem"
root_directory = "{}"

[blob_storage.routes]
user_attachment = "primary"
tool_artifact = "primary"
imported_source = "primary"
generated_artifact = "primary"
"#,
            self.staging.display(),
            self.store.display(),
        )
    }
}

pub(crate) struct CommittedBlobReadFixture {
    pub(crate) runtime: RunningRuntime,
    pub(crate) connection: Connection,
    pub(crate) bytes: &'static [u8],
    pub(crate) digest: BlobDigest,
    pub(crate) wire_digest: CanonicalBlobDigest,
    pub(crate) expected_length: CanonicalU64,
}

impl CommittedBlobReadFixture {
    pub(crate) async fn start(bytes: &'static [u8]) -> Result<Self, Box<dyn Error>> {
        Self::from_runtime(RunningRuntime::start_with_blob_storage().await?, bytes).await
    }

    pub(crate) async fn from_runtime(
        runtime: RunningRuntime,
        bytes: &'static [u8],
    ) -> Result<Self, Box<dyn Error>> {
        let digest = BlobDigest::digest(bytes);
        let wire_digest = CanonicalBlobDigest::from_digest(digest);
        let expected_length = CanonicalU64::new(u64::try_from(bytes.len())?);
        let mut connection = Connection::connect(runtime.socket()).await?;
        commit_blob_upload(&mut connection, wire_digest, expected_length, bytes).await?;
        Ok(Self {
            runtime,
            connection,
            bytes,
            digest,
            wire_digest,
            expected_length,
        })
    }

    pub(crate) fn object_path(&self) -> PathBuf {
        self.runtime
            .blob_storage_root
            .as_ref()
            .expect("the fixture owns one blob store")
            .store
            .join(BlobObjectKey::for_digest(self.digest).as_str())
    }

    pub(crate) fn expected_replica_count(&self) -> CanonicalU64 {
        CanonicalU64::new(1)
    }

    pub(crate) fn expected_range(
        &self,
        offset_bytes: CanonicalU64,
        length_bytes: CanonicalU64,
    ) -> &'static [u8] {
        let offset =
            usize::try_from(offset_bytes.value()).expect("the fixture range offset fits in usize");
        let length =
            usize::try_from(length_bytes.value()).expect("the fixture range length fits in usize");
        let end = offset
            .checked_add(length)
            .expect("the fixture range end is representable");
        self.bytes
            .get(offset..end)
            .expect("the fixture contains the expected range")
    }

    pub(crate) async fn stop(self) -> Result<(), Box<dyn Error>> {
        drop(self.connection);
        self.runtime.stop().await
    }
}

pub(crate) async fn create_alias_session(
    connection: &mut Connection,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    create_alias_session_with(
        connection,
        CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        SessionPlacement::Pathless {},
    )
    .await
}

pub(crate) async fn create_alias_session_with(
    connection: &mut Connection,
    alias_id: CanonicalUuid,
    placement: SessionPlacement,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    connection
        .request(
            1,
            ClientRequest::CreateSession {
                command_id: command()?,
                initial_model_selection: ModelSelection::Alias { alias_id },
                model_settings: ModelSettingsOverlay::inherit_all(),
                system_prompt: SystemPromptMember::present(None),
                placement,
                lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
            },
        )
        .await?;
    match connection.response().await?.message() {
        ServerMessage::SessionCreated { session_id, .. } => Ok(*session_id),
        message => Err(io::Error::other(format!(
            "unexpected create-session fixture response: {message:?}"
        ))
        .into()),
    }
}

pub(crate) async fn submit_first_input(
    connection: &mut Connection,
    session_id: CanonicalUuid,
    content: String,
) -> Result<(CanonicalUuid, CanonicalUuid), Box<dyn Error>> {
    connection
        .request(
            2,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(content),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    match connection.response().await?.message() {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            accepted_input_id,
            acceptance_position,
            turn_id,
            ..
        } if *submitted_session == session_id && acceptance_position.value() == 1 => {
            Ok((*accepted_input_id, *turn_id))
        }
        message => Err(io::Error::other(format!(
            "unexpected first-input fixture response: {message:?}"
        ))
        .into()),
    }
}

/// Reads one accepted-input acknowledgement and returns the successor turn it
/// names, requiring the exact session and acceptance ordinal the caller states.
pub(crate) async fn accepted_successor_turn(
    connection: &mut Connection,
    session_id: CanonicalUuid,
    acceptance: u64,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    match response_within(connection).await?.message() {
        ServerMessage::InputSubmitted {
            session_id: accepted_session,
            acceptance_position,
            turn_id,
            ..
        } if *accepted_session == session_id && acceptance_position.value() == acceptance => {
            Ok(*turn_id)
        }
        message => {
            Err(io::Error::other(format!("unexpected accepted-input response: {message:?}")).into())
        }
    }
}

pub(crate) async fn accepted_successor_model_settings(
    connection: &mut Connection,
    session_id: CanonicalUuid,
    acceptance: u64,
) -> Result<ModelSettingsSnapshot, Box<dyn Error>> {
    match response_within(connection).await?.message() {
        ServerMessage::InputSubmitted {
            session_id: accepted_session,
            acceptance_position,
            model_settings,
            ..
        } if *accepted_session == session_id && acceptance_position.value() == acceptance => {
            Ok(*model_settings)
        }
        message => Err(io::Error::other(format!(
            "unexpected accepted-input settings response: {message:?}"
        ))
        .into()),
    }
}

pub(crate) async fn response_within(
    connection: &mut Connection,
) -> Result<ServerFrame, Box<dyn Error>> {
    timeout(RESPONSE_ALLOWANCE, connection.response()).await?
}

pub(crate) async fn attach_empty_follower(
    socket: &Path,
    version: ProtocolVersion,
    request_id: u64,
    session_id: CanonicalUuid,
) -> Result<Connection, Box<dyn Error>> {
    let mut follow = Connection::connect(socket).await?;
    follow
        .request_version(
            version,
            request_id,
            ClientRequest::FollowSession { session_id },
        )
        .await?;
    assert!(matches!(
           response_within(&mut follow).await?.message(),
           ServerMessage::TranscriptSnapshotStart {
    workspace_root_kind: None,
               session_id: snapshot_session,
               ..
           } if *snapshot_session == session_id
       ));
    assert!(matches!(
        response_within(&mut follow).await?.message(),
        ServerMessage::TranscriptModelCallsEnd { model_call_count }
            if model_call_count.value() == 0
    ));
    assert!(matches!(
        response_within(&mut follow).await?.message(),
        ServerMessage::TranscriptSnapshotEnd {
            session_id: snapshot_session,
            turn_count,
            entry_count,
            ..
        } if *snapshot_session == session_id
            && turn_count.value() == 0
            && entry_count.value() == 0
    ));
    Ok(follow)
}

/// The durable turn shape one scheduler pass is expected to leave behind.
#[derive(Clone, Copy)]
pub(crate) enum TurnSettle {
    /// The turn reached its terminal lifecycle state.
    Terminal,
    /// The turn parked on an unstopped ambiguous model call and still holds
    /// its slot.
    ParkedOnAmbiguity,
}

impl TurnSettle {
    pub(crate) const fn predicate_sql(self) -> &'static str {
        match self {
            Self::Terminal => {
                "SELECT EXISTS (
                    SELECT 1
                      FROM turn_lifecycle
                     WHERE session_id = $1
                       AND turn_id = $2
                       AND state_kind = 'terminal'
                )"
            }
            Self::ParkedOnAmbiguity => {
                "SELECT EXISTS (
                    SELECT 1
                      FROM turn_lifecycle
                     WHERE session_id = $1
                       AND turn_id = $2
                       AND state_kind = 'active'
                       AND active_phase_kind = 'awaiting_model_call_recovery'
                )"
            }
        }
    }
}

pub(crate) async fn wait_for_turn_settle(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
    settle: TurnSettle,
) {
    loop {
        let settled: bool = sqlx::query_scalar(settle.predicate_sql())
            .bind(session.into_uuid())
            .bind(turn.into_uuid())
            .fetch_one(pool)
            .await
            .unwrap_or(false);
        if settled {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub(crate) async fn execute_streamed_turn(
    runtime: &mut RunningRuntime,
    scripted: ScriptedModel<ModelCallId>,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<ScriptedModel<ModelCallId>, Box<dyn Error>> {
    execute_streamed_turn_until(runtime, scripted, session_id, turn_id, TurnSettle::Terminal).await
}

pub(crate) async fn execute_streamed_turn_until(
    runtime: &mut RunningRuntime,
    scripted: ScriptedModel<ModelCallId>,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    settle: TurnSettle,
) -> Result<ScriptedModel<ModelCallId>, Box<dyn Error>> {
    let model_configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    execute_streamed_turn_until_with_configuration(
        runtime,
        scripted,
        model_configuration,
        session_id,
        turn_id,
        settle,
    )
    .await
}

pub(crate) async fn execute_streamed_turn_until_with_configuration(
    runtime: &mut RunningRuntime,
    scripted: ScriptedModel<ModelCallId>,
    model_configuration: HubModelConfiguration,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    settle: TurnSettle,
) -> Result<ScriptedModel<ModelCallId>, Box<dyn Error>> {
    let probe = scripted.clone();
    let provider =
        RuntimeModelCallProvider::new(scripted, model_configuration.runtime_model_catalog(), None)
            .with_text_delta_sink(runtime.provider_text_delta_sink());
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(signalboxd::WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                PostgresModelCallRepository::new(
                    runtime.pool.clone(),
                    model_configuration.target_catalog(),
                    ModelCallCredentialReference::new("streaming-fixture"),
                ),
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, Vec::new()),
        ));
    let pass = ActivatedTurnPass::new(
        StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(runtime.pool.clone()),
        ),
        execution,
    );
    let mut scheduler = SchedulerLoop::new(runtime.take_work_source(), pass);
    let observation_pool = runtime.pool.clone();
    let session = SessionId::from_uuid(session_id.into_uuid());
    let turn = TurnId::from_uuid(turn_id.into_uuid());
    let fatal_shutdown = fatal_execution.clone();
    let shutdown = async move {
        tokio::select! {
            () = wait_for_turn_settle(&observation_pool, session, turn, settle) => {}
            () = fatal_shutdown.wait() => {}
        }
    };
    assert_eq!(
        timeout(RUNTIME_SETTLE_ALLOWANCE, scheduler.run_until(shutdown)).await?,
        SchedulerLoopExit::Shutdown
    );
    assert!(!fatal_execution.is_triggered());
    Ok(probe)
}

pub(crate) fn completed_script(provider_model: &str, text: &str, usage: TokenUsage) -> Script {
    Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new(provider_model)),
        finish: CompletionFinish::EndTurn,
        content: vec![AssistantPart::Text(text.to_owned())],
        usage,
    }))
}

#[track_caller]
pub(crate) fn submitted_session(message: &ServerMessage) -> CanonicalUuid {
    match message {
        ServerMessage::InputSubmitted { session_id, .. } => *session_id,
        message => panic!("fixture expected input-submitted, got {message:?}"),
    }
}

#[track_caller]
pub(crate) fn transcript_snapshot_start_cursor(
    message: &ServerMessage,
    expected_session: CanonicalUuid,
) -> u64 {
    match message {
        ServerMessage::TranscriptSnapshotStart {
            workspace_root_kind: None,
            session_id,
            cursor,
            ..
        } if *session_id == expected_session => cursor.value(),
        message => panic!("fixture expected transcript-snapshot start, got {message:?}"),
    }
}

#[track_caller]
pub(crate) fn transcript_turn_projection(
    message: &ServerMessage,
) -> (CanonicalUuid, u64, TurnState) {
    match message {
        ServerMessage::TranscriptTurn {
            turn_id,
            acceptance_position,
            state,
            ..
        } => (*turn_id, acceptance_position.value(), state.clone()),
        message => panic!("fixture expected transcript-turn projection, got {message:?}"),
    }
}

#[track_caller]
pub(crate) fn transcript_model_call_count(message: &ServerMessage) -> u64 {
    match message {
        ServerMessage::TranscriptModelCallsEnd { model_call_count } => model_call_count.value(),
        message => panic!("fixture expected transcript-model-calls end, got {message:?}"),
    }
}

#[track_caller]
pub(crate) fn protocol_error_code(message: &ServerMessage) -> ErrorCode {
    match message {
        ServerMessage::Error { code, .. } => *code,
        message => panic!("fixture expected protocol error, got {message:?}"),
    }
}

#[track_caller]
pub(crate) fn protocol_error_detail(message: &ServerMessage) -> Option<RejectionDetail> {
    match message {
        ServerMessage::Error { detail, .. } => detail.value(),
        message => panic!("fixture expected protocol error detail, got {message:?}"),
    }
}

pub(crate) async fn activate_turn(pool: &PgPool, session: SessionId) -> Result<(), Box<dyn Error>> {
    let mut service = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) = service.execute(session).await? else {
        return Err(io::Error::other("the fixture turn must activate").into());
    };
    let recorded = signalboxd::WorkspaceInstructionRuntime::new(pool.clone(), None, Vec::new())
        .prepare(session, activated.turn())
        .await?;
    if !recorded {
        return Err(io::Error::other("the fixture instruction manifest must record").into());
    }
    Ok(())
}

/// Commits a confirm-classified tool round over the issued fixture call, so
/// the active turn parks on the approval wait for the first named request.
pub(crate) async fn park_turn_on_tool_approval(
    pool: &PgPool,
    session_id: CanonicalUuid,
    request_ids: &[CanonicalUuid],
) -> Result<(), Box<dyn Error>> {
    let session = SessionId::from_uuid(session_id.into_uuid());
    let (calls, authorized, _) = authorize_issued_model_call(pool, session_id).await?;
    let response = ToolUsingAssistantResponse::try_from_parts(
        request_ids
            .iter()
            .map(|_| {
                AssistantResponsePart::ToolCall(ToolCallProposal::new(
                    ToolName::try_new(String::from("confirmed"))
                        .expect("the fixture tool name is valid"),
                    NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                        .expect("the fixture arguments are bounded"),
                ))
            })
            .collect(),
    )
    .expect("the fixture proposals form a tool-using response");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
            response,
            retained_input_tokens: None,
            retained_output_tokens: None,
        });
    let identities = request_ids
        .iter()
        .map(|request_id| {
            ToolResponsePartIdentity::tool_call(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ToolRequestId::from_uuid(request_id.into_uuid()),
                InitialToolApproval::Confirm,
            )
        })
        .collect();
    let outcome = calls
        .apply_terminal_observation(
            session,
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                identities,
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                None,
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    let first_request = request_ids
        .first()
        .map(|request_id| ToolRequestId::from_uuid(request_id.into_uuid()));
    assert!(
        matches!(
            outcome,
            ModelCallTerminalOutcome::ToolRound(ref round)
                if matches!(
                    round.next_phase(),
                    ActiveTurnPhase::AwaitingApproval { request: waiting }
                        if Some(*waiting) == first_request
                )
        ),
        "the confirm-classified round must park on its first request"
    );
    Ok(())
}

/// Reads one complete transcript snapshot and returns every message between
/// its validated start and end frames.
pub(crate) async fn read_transcript_messages(
    connection: &mut Connection,
    request_id: u64,
    session_id: CanonicalUuid,
) -> Result<Vec<ServerMessage>, Box<dyn Error>> {
    connection
        .request_version(
            ProtocolVersion::One,
            request_id,
            ClientRequest::ReadTranscript { session_id },
        )
        .await?;
    let start = response_within(connection).await?;
    assert!(matches!(
           start.message(),
           ServerMessage::TranscriptSnapshotStart {
    workspace_root_kind: None,
               session_id: snapshot_session,
               ..
           } if *snapshot_session == session_id
       ));
    let mut messages = Vec::new();
    loop {
        let frame = response_within(connection).await?;
        if let ServerMessage::TranscriptSnapshotEnd {
            session_id: end_session,
            ..
        } = frame.message()
        {
            assert_eq!(*end_session, session_id);
            return Ok(messages);
        }
        messages.push(frame.message().clone());
    }
}

#[track_caller]
pub(crate) fn turn_state_of(messages: &[ServerMessage], selected_turn: CanonicalUuid) -> TurnState {
    messages
        .iter()
        .find_map(|message| match message {
            ServerMessage::TranscriptTurn { turn_id, state, .. } if *turn_id == selected_turn => {
                Some(state.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("the snapshot must project turn {selected_turn}"))
}

#[track_caller]
pub(crate) fn rejected_detail(message: &ServerMessage) -> RejectionDetail {
    match message {
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            detail,
            ..
        } => detail
            .value()
            .expect("a rejected error carries its typed detail"),
        message => panic!("fixture expected a rejected error, got {message:?}"),
    }
}

#[track_caller]
pub(crate) fn decided_receipt(message: &ServerMessage) -> (CanonicalUuid, ToolDecision) {
    match message {
        ServerMessage::ToolRequestDecided {
            tool_request_id,
            decision,
        } => (*tool_request_id, decision.clone()),
        message => panic!("fixture expected a decision receipt, got {message:?}"),
    }
}

impl std::fmt::Display for CommitAmbiguousCountFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("guarded count acknowledgement was lost")
    }
}

impl Error for CommitAmbiguousCountFailure {}

impl ClassifyOperatorFailure for CommitAmbiguousCountFailure {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::Infrastructure {
            commit_ambiguous: true,
        }
    }
}
