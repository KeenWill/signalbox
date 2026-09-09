use super::*;

impl<'a> Output<'a> {
    pub(crate) fn review_orchestration(
        &mut self,
        snapshot: &ReviewOrchestrationSnapshot,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "attempt={} target={} state={} concerns={} findings={} judgment_members={} \
             judgment_effects_applied={} repairs_fixed={} publications_published={}",
            snapshot.attempt_id,
            snapshot.target_id,
            review_orchestration_state_label(snapshot.state),
            snapshot.concerns.len(),
            snapshot.counts.finding_count.value(),
            snapshot.counts.judgment_member_count.value(),
            snapshot.counts.judgment_effect_applied_count.value(),
            snapshot.counts.repair_fixed_count.value(),
            snapshot.counts.publication_published_count.value(),
        )?;
        self.text_field("concern_set_version", &snapshot.concern_set_version)?;
        writeln!(
            self.stdout,
            "template_import_digest={} template_judgment_digest={} template_repair_digest={} \
             template_publication_digest={}",
            snapshot.stage_template_digests.import.as_str(),
            snapshot.stage_template_digests.judgment.as_str(),
            snapshot.stage_template_digests.repair.as_str(),
            snapshot.stage_template_digests.publication.as_str(),
        )?;
        for (index, concern) in snapshot.concerns.iter().enumerate() {
            writeln!(
                self.stdout,
                "concern_index={index} status={} pass={} template_digest={}",
                review_orchestration_concern_status_label(concern.status),
                concern
                    .pass_id
                    .map_or_else(|| String::from("-"), |id| id.to_string()),
                concern.template_digest.as_str(),
            )?;
            self.text_field("concern_key", &concern.key)?;
        }
        Ok(())
    }

    pub(crate) fn review_target(&mut self, target: &ReviewTargetSnapshot) -> io::Result<()> {
        let subject = match target.subject {
            ReviewTargetSubject::ChangeRequest { number } => {
                format!("change_request:{}", number.value())
            }
            ReviewTargetSubject::Commit {} => String::from("commit"),
        };
        writeln!(
            self.stdout,
            "target={} subject={} parent={}",
            target.target_id,
            subject,
            target
                .stack_parent_target_id
                .map_or_else(|| String::from("-"), |id| id.to_string()),
        )?;
        self.text_field("provider", &target.provider)?;
        self.text_field("repository", &target.repository)?;
        self.text_field("head_revision", &target.head_revision)?;
        match target.base_revision.as_deref() {
            Some(base_revision) => {
                writeln!(self.stdout, "base_revision_present=true")?;
                self.text_field("base_revision", base_revision)
            }
            None => writeln!(self.stdout, "base_revision_present=false"),
        }
    }

    pub(crate) fn review_run(
        &mut self,
        run: &ReviewRunSnapshot,
        pass: Option<&signalbox_process_protocol::ReviewPassSnapshot>,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "run={} target={} workflow={} policy_version={} minimum_judge_confidence={} \
             minimum_publication_confidence={} state={} pass={}",
            run.run_id,
            run.target_id,
            review_workflow_label(run.workflow),
            run.policy_version.value(),
            run.minimum_judge_confidence.value(),
            run.minimum_publication_confidence.value(),
            review_run_state_label(run.state),
            run.pass_id
                .map_or_else(|| String::from("-"), |id| id.to_string()),
        )?;
        if let Some(pass) = pass {
            writeln!(
                self.stdout,
                "pass={} kind={} state={} session={} input={} origin_turn={} turn={} frontier={}",
                pass.pass_id,
                review_pass_kind_label(pass.kind),
                review_pass_state_label(pass.state),
                pass.session_id,
                pass.accepted_input_id,
                pass.origin_turn_id,
                pass.turn_id
                    .map_or_else(|| String::from("-"), |id| id.to_string()),
                pass.output_frontier_id
                    .map_or_else(|| String::from("-"), |id| id.to_string()),
            )?;
        }
        Ok(())
    }

    pub(crate) fn review_finding(&mut self, finding: &ReviewFindingSnapshot) -> io::Result<()> {
        writeln!(
            self.stdout,
            "finding={} target={} run={} pass={} status={} events={} line_start={} line_end={} \
             diff_side={} severity={} is_real_confidence={} severity_label_confidence={}",
            finding.finding.finding_id,
            finding.target_id,
            finding.run_id,
            finding.producing_pass_id,
            review_finding_status_label(finding.status),
            finding.event_count.value(),
            finding
                .finding
                .line_start
                .map_or_else(|| String::from("none"), |line| line.value().to_string()),
            finding
                .finding
                .line_end
                .map_or_else(|| String::from("none"), |line| line.value().to_string()),
            finding
                .finding
                .diff_side
                .map_or("none", review_diff_side_label),
            review_severity_label(finding.finding.severity),
            finding.finding.is_real_confidence.value(),
            finding.finding.severity_label_confidence.value(),
        )?;
        self.text_field("file_path", &finding.finding.file_path)?;
        self.text_field("title", &finding.finding.title)?;
        self.text_field("body", &finding.finding.body)?;
        self.text_field("category", &finding.finding.category)?;
        match finding.finding.recommended_fix.as_deref() {
            Some(recommended_fix) => {
                writeln!(self.stdout, "recommended_fix_present=true")?;
                self.text_field("recommended_fix", recommended_fix)
            }
            None => writeln!(self.stdout, "recommended_fix_present=false"),
        }
    }

    pub(crate) fn snapshot(
        &mut self,
        snapshot: &mut TranscriptSnapshot,
    ) -> Result<(), ClientError> {
        let mut rendered_snapshot = tempfile::tempfile()?;
        {
            let mut staged = Output::new(&mut rendered_snapshot, &mut *self.stderr, self.raw);
            staged.snapshot_repository_watch(snapshot.repository_watch())?;
            staged.snapshot_workspace_root(snapshot.workspace_root_kind())?;
            staged.snapshot_runner(snapshot.runner())?;
            staged.render_snapshot(snapshot, None, SnapshotSelection::All, true)?;
            staged.render_usage(snapshot)?;
        }
        rendered_snapshot.seek(SeekFrom::Start(0))?;
        io::copy(&mut rendered_snapshot, &mut self.stdout)?;
        Ok(())
    }

    pub(crate) fn followed_snapshot(
        &mut self,
        snapshot: &mut TranscriptSnapshot,
        displayed: &mut SnapshotIdentitySet,
    ) -> Result<(), ClientError> {
        self.snapshot_repository_watch(snapshot.repository_watch())?;
        self.snapshot_workspace_root(snapshot.workspace_root_kind())?;
        self.snapshot_runner(snapshot.runner())?;
        self.render_snapshot(snapshot, Some(displayed), SnapshotSelection::All, true)
    }

    fn snapshot_workspace_root(
        &mut self,
        kind: Option<signalbox_process_protocol::SessionWorkspaceRootKind>,
    ) -> io::Result<()> {
        use signalbox_process_protocol::SessionWorkspaceRootKind;
        if let Some(kind) = kind {
            writeln!(
                self.stdout,
                "workspace_root_kind={}",
                match kind {
                    SessionWorkspaceRootKind::Derived => "derived",
                    SessionWorkspaceRootKind::Configured => "configured",
                    SessionWorkspaceRootKind::Provisioned => "provisioned",
                }
            )?;
        }
        Ok(())
    }

    fn snapshot_repository_watch(
        &mut self,
        origin: Option<&signalbox_process_protocol::RepositoryWatchProvenance>,
    ) -> io::Result<()> {
        let Some(origin) = origin else {
            return Ok(());
        };
        write!(
            self.stdout,
            "creation_cause=module_dispatched actor=repo_watch repository={} rule={} revision={} dispatch={} action={} event={} event_kind={:?}",
            self.render_field(&origin.repository, TextField::DelimitedOnLine),
            self.render_field(&origin.rule_id, TextField::DelimitedOnLine),
            origin.rule_revision.value(),
            origin.dispatch_id,
            origin.action_ordinal.value(),
            origin.event_id,
            origin.event_kind
        )?;
        if let Some(number) = origin.pull_request {
            write!(self.stdout, " pull_request={}", number.value())?;
        }
        writeln!(self.stdout)
    }

    fn snapshot_runner(&mut self, runner: Option<&RunnerProjection>) -> io::Result<()> {
        let Some(runner) = runner else {
            return Ok(());
        };
        write!(self.stdout, "runner_snapshot selector=")?;
        match runner.selector() {
            RunnerProjectionSelector::Runner { runner_id } => {
                write!(self.stdout, "runner selector_runner={runner_id}")?;
            }
            RunnerProjectionSelector::CapabilityClass { name } => write!(
                self.stdout,
                "capability_class selector_capability={}",
                self.render_field(name.as_str(), TextField::DelimitedOnLine)
            )?,
        }
        if let Some(runner_id) = runner.runner_id() {
            write!(self.stdout, " runner={runner_id}")?;
        }
        write!(
            self.stdout,
            " placement_revision={} sandbox={}",
            runner.placement_revision().value(),
            runner_sandbox_profile(runner.sandbox_profile())
        )?;
        if let Some(profile) = runner.credential_profile() {
            write!(
                self.stdout,
                " credential_profile={}",
                self.render_field(profile.as_str(), TextField::DelimitedOnLine)
            )?;
        }
        if let Some(repository) = runner.repository() {
            write!(
                self.stdout,
                " repository={}",
                self.render_field(repository.as_str(), TextField::DelimitedOnLine)
            )?;
        }
        if let Some(directory) = runner.working_directory() {
            write!(
                self.stdout,
                " working_directory={}",
                self.render_field(directory.as_str(), TextField::DelimitedOnLine)
            )?;
        }
        if let Some(health) = runner.connection_health() {
            write!(
                self.stdout,
                " connection_health={}",
                runner_connection_health(health)
            )?;
        }
        writeln!(
            self.stdout,
            " state={}",
            runner_projection_state(runner.state())
        )
    }

    pub(crate) fn terminal_material(
        &mut self,
        snapshot: &mut TranscriptSnapshot,
        displayed: &mut SnapshotIdentitySet,
        selection: SnapshotSelection,
    ) -> Result<(), ClientError> {
        self.render_snapshot(snapshot, Some(displayed), selection, false)
    }

    fn render_snapshot(
        &mut self,
        snapshot: &mut TranscriptSnapshot,
        mut displayed: Option<&mut SnapshotIdentitySet>,
        selection: SnapshotSelection,
        render_turns: bool,
    ) -> Result<(), ClientError> {
        let selection_context = selection.context(snapshot)?;
        let mut render_content = false;
        for record in snapshot.replay()? {
            match record? {
                SnapshotRecord::Turn(turn) if render_turns => self.snapshot_turn(&turn)?,
                SnapshotRecord::Turn(_) => {}
                SnapshotRecord::ModelCallUsage(_) => {}
                SnapshotRecord::Entry(entry) => {
                    render_content = false;
                    let selected = selection.includes(&entry, &selection_context);
                    let undisplayed = if selected {
                        match displayed.as_deref_mut() {
                            Some(identities) => {
                                identities.insert(entry.source_session_id, entry.entry_id)?
                            }
                            None => true,
                        }
                    } else {
                        false
                    };
                    if undisplayed {
                        render_content = matches!(entry.kind, SnapshotEntryKind::Text(_));
                        self.snapshot_entry(&entry)?;
                    }
                }
                SnapshotRecord::Content(content) if render_content => {
                    let content_ends_with_newline = content.content.as_str().ends_with('\n');
                    self.text_fragment(
                        content.content.as_str(),
                        content.final_fragment,
                        content_ends_with_newline,
                    )?;
                    if content.final_fragment {
                        render_content = false;
                    }
                }
                SnapshotRecord::Content(_) => {}
            }
        }
        Ok(())
    }

    fn render_usage(&mut self, snapshot: &mut TranscriptSnapshot) -> Result<(), ClientError> {
        let mut rendered_usage = tempfile::tempfile()?;
        let mut current_turn: Option<(CanonicalUuid, UsageAggregate)> = None;
        let mut session_total = UsageAggregate::new()?;
        for record in snapshot.replay()? {
            let SnapshotRecord::ModelCallUsage(evidence) = record? else {
                continue;
            };
            if current_turn
                .as_ref()
                .is_some_and(|(turn, _)| *turn != evidence.turn_id)
            {
                let (turn, mut total) = current_turn.take().ok_or(ClientError::Protocol(
                    "token usage turn grouping was invalid",
                ))?;
                self.usage_lines(&mut rendered_usage, Some(turn), &mut total)?;
            }
            if current_turn.is_none() {
                current_turn = Some((evidence.turn_id, UsageAggregate::new()?));
            }
            let (_, turn_total) = current_turn.as_mut().ok_or(ClientError::Protocol(
                "token usage turn grouping was invalid",
            ))?;
            turn_total.add(&evidence)?;
            session_total.add(&evidence)?;
        }
        if let Some((turn, mut total)) = current_turn {
            self.usage_lines(&mut rendered_usage, Some(turn), &mut total)?;
        }
        self.usage_lines(&mut rendered_usage, None, &mut session_total)?;
        rendered_usage.seek(SeekFrom::Start(0))?;
        io::copy(&mut rendered_usage, &mut self.stdout)?;
        Ok(())
    }

    fn usage_lines<OutputWriter: Write>(
        &self,
        stdout: &mut OutputWriter,
        turn: Option<CanonicalUuid>,
        total: &mut UsageAggregate,
    ) -> Result<(), ClientError> {
        Self::usage_line(stdout, turn, UsageProvenance::Reported, total.reported)?;
        Self::usage_line(stdout, turn, UsageProvenance::Estimated, total.estimated)?;
        for index in 0..total.costs.capacity {
            let Some((key, cost)) = total.costs.entry_at(index)? else {
                continue;
            };
            let prefix = turn.map_or_else(
                || String::from("cost_total scope=session"),
                |turn| format!("cost turn={turn}"),
            );
            let rate_version = self.render_field(&key.rate_version, TextField::DelimitedOnLine);
            writeln!(
                stdout,
                "{prefix} usage_provenance={} label={} rate_version={} usd={} costed_calls={}",
                usage_provenance_label(key.provenance),
                cost_label(key.label),
                rate_version,
                cost.amount_usd.normalize(),
                cost.calls,
            )?;
        }
        Ok(())
    }

    fn usage_line<OutputWriter: Write>(
        stdout: &mut OutputWriter,
        turn: Option<CanonicalUuid>,
        provenance: UsageProvenance,
        total: TokenUsageTotal,
    ) -> io::Result<()> {
        let prefix = turn.map_or_else(
            || String::from("usage_total scope=session"),
            |turn| format!("usage turn={turn}"),
        );
        writeln!(
            stdout,
            "{prefix} usage_provenance={} terminal_calls={} input_tokens={} \
             input_tokens_present_calls={}/{} \
             output_tokens={} output_tokens_present_calls={}/{} \
             cache_creation_input_tokens={} \
             cache_creation_input_tokens_present_calls={}/{} cache_read_input_tokens={} \
             cache_read_input_tokens_present_calls={}/{}",
            usage_provenance_label(provenance),
            total.terminal_calls,
            total.input.label(),
            total.input.present_calls,
            total.terminal_calls,
            total.output.label(),
            total.output.present_calls,
            total.terminal_calls,
            total.cache_creation_input.label(),
            total.cache_creation_input.present_calls,
            total.terminal_calls,
            total.cache_read_input.label(),
            total.cache_read_input.present_calls,
            total.terminal_calls,
        )
    }
}
