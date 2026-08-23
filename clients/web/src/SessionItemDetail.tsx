import { useQuery } from '@tanstack/react-query'
import { type ReactNode, useEffect, useState } from 'react'
import type {
  WebSessionTimelineDetailPage,
  WebSessionTimelineWindow,
} from './generated/web-contract.mjs'
import type { HttpSessionTimelineSource } from './session-timeline/model'

// Effective request limits: keep each independently requested detail page small while allowing
// explicit continuation within the daemon-advertised hard ceilings.
const DETAIL_PAGE_ITEMS = 1
const DETAIL_PAGE_BYTES = 16 * 1024
// Hard render ceilings: cap repeated members even inside a valid bounded page so one record cannot
// mount an unexpectedly large subtree.
const MAX_RENDERED_ATTACHMENTS = 24
const MAX_RENDERED_MEMBERS = 24

type DetailItem = WebSessionTimelineDetailPage['items'][number]
type DetailBody = DetailItem['body']
type TextExcerpt = Extract<DetailBody, { type: 'user_input' }>['text']
type ModelCallBody = Extract<DetailBody, { type: 'model_call' }>
type GoalEvent = Extract<DetailBody, { type: 'goal_event' }>['event']
type ToolAttempt = Extract<DetailBody, { type: 'tool_batch' }>['tools'][number]
type DelegationDetail = Extract<DetailBody, { type: 'delegation' }>['detail']
type DetailKind = DetailItem['kind']

const delegationUpdateKinds = new Set([
  'child_spawned',
  'child_waiting',
  'child_lifecycle_disposition',
  'child_result',
  'session_message',
])
const delegationWakeKinds = new Set(['result_wake', 'message_wake'])
const goalEventKinds = new Set([
  'commissioned',
  'blocked',
  'resumed',
  'achieved',
  'user_stopped',
  'superseded',
])
const goalBlockedReasons = new Set([
  'user_input_required',
  'external_change_required',
  'authorization_required',
  'execution_failure',
])

const modelFailureCauses = new Set([
  'credential_rejected',
  'permission_denied',
  'invalid_request',
  'target_not_found',
  'request_too_large',
  'rate_limited',
  'quota_exhausted',
  'overloaded',
  'provider_internal',
  'unrecognized',
])
const toolFailureCauses = new Set([
  'unknown_tool',
  'invalid_arguments',
  'execution_failed',
  'result_too_large',
  'crash_lost',
])
const hasCompatibleGoalReason = (event: GoalEvent): boolean =>
  goalEventKinds.has(event.type) &&
  (event.type !== 'blocked' || goalBlockedReasons.has(event.reason)) &&
  (event.type === 'user_stopped' ? !('text' in event) : 'text' in event)

const positiveDecimal = (value: string): boolean => /^[1-9][0-9]*$/.test(value)

const compatibleDelegationOutcome = (detail: DelegationDetail): boolean => {
  if (detail.type !== 'child_lifecycle_disposition' && detail.type !== 'child_result') return true
  const childProvenance = detail.provenance.type === 'child_turn'
  const parentProvenance =
    detail.provenance.type === 'parent_turn_command' ||
    detail.provenance.type === 'parent_goal_command'
  const pairMatches =
    (detail.outcome === 'result_returned' &&
      detail.reason === 'child_completed' &&
      childProvenance) ||
    (detail.outcome === 'child_failed' &&
      (detail.reason === 'child_execution_failed' ||
        detail.reason === 'child_result_unavailable') &&
      childProvenance) ||
    (detail.outcome === 'child_cancelled' &&
      detail.reason === 'child_cancelled' &&
      childProvenance) ||
    (detail.outcome === 'child_stopped' &&
      detail.reason === 'parent_stopped_with_descendants' &&
      parentProvenance) ||
    (detail.outcome === 'child_cancelled' &&
      detail.reason === 'parent_cancelled_with_descendants' &&
      parentProvenance) ||
    ((detail.outcome === 'continue_running' || detail.outcome === 'already_terminal') &&
      (detail.reason === 'parent_stopped_with_descendants' ||
        detail.reason === 'parent_cancelled_with_descendants') &&
      parentProvenance)
  if (!pairMatches) return false
  if (
    detail.provenance.type === 'parent_goal_command' &&
    !positiveDecimal(detail.provenance.goal_generation)
  ) {
    return false
  }
  return detail.type !== 'child_result'
    ? true
    : detail.outcome === 'result_returned'
      ? detail.content != null
      : detail.content == null
}

const compatibleKinds = {
  session_created: ['session_created'],
  model_settings: ['session_model_settings_changed', 'turn_model_settings_resolved'],
  user_input: ['input_accepted'],
  model_call: ['model_call_transition'],
  tool_batch: ['tool_batch_transition'],
  tool_approval_decision: ['tool_approval_decided'],
  goal_event: ['goal_turn_retired'],
  context_compaction: ['context_compacted'],
  turn_lifecycle: [
    'turn_activated',
    'turn_failed',
    'turn_completed',
    'turn_refused',
    'turn_cancelled',
  ],
  reconciliation: ['turn_reconciliation_required'],
  runner: ['runner_state_transition'],
  delegation: ['delegation_update', 'delegation_wake'],
} as const satisfies Record<DetailBody['type'], readonly DetailKind[]>

export const isCompatibleDetailBody = (
  kind: DetailKind,
  body: DetailBody,
  sessionId?: string,
): boolean => {
  if (body.type === 'model_settings') {
    const subtypeMatches =
      kind === 'session_model_settings_changed'
        ? body.detail.type === 'session_defaults_changed'
        : kind === 'turn_model_settings_resolved'
          ? body.detail.type === 'turn_resolved'
          : false
    if (!subtypeMatches) return false
    return body.detail.type === 'session_defaults_changed'
      ? positiveDecimal(body.detail.prior_defaults_version) &&
          BigInt(body.detail.installed_defaults_version) ===
            BigInt(body.detail.prior_defaults_version) + 1n
      : positiveDecimal(body.detail.defaults_version)
  }
  if (body.type === 'turn_lifecycle') {
    const lifecycleByKind = {
      turn_activated: ['activated', 'activated'],
      turn_failed: ['terminalized', 'failed'],
      turn_completed: ['terminalized', 'completed'],
      turn_refused: ['terminalized', 'refused'],
      turn_cancelled: ['terminalized', 'cancelled'],
    } as const
    const expected = lifecycleByKind[kind as keyof typeof lifecycleByKind]
    return expected?.[0] === body.lifecycle && expected[1] === body.cause_code
  }
  if (body.type === 'delegation') {
    const subtypeMatches =
      kind === 'delegation_update'
        ? delegationUpdateKinds.has(body.detail.type)
        : kind === 'delegation_wake'
          ? delegationWakeKinds.has(body.detail.type)
          : false
    if (!subtypeMatches || !compatibleDelegationOutcome(body.detail)) return false
    if ('child_session_id' in body.detail && body.detail.child_session_id === sessionId)
      return false
    if (
      body.detail.type === 'child_lifecycle_disposition' &&
      !positiveDecimal(body.detail.event_ordinal)
    ) {
      return false
    }
    if (body.detail.type === 'session_message') {
      return (
        positiveDecimal(body.detail.message_ordinal) &&
        positiveDecimal(body.detail.delivery_sequence) &&
        (sessionId === undefined || body.detail.recipient_session_id === sessionId) &&
        body.detail.sender_session_id !== body.detail.recipient_session_id
      )
    }
    return true
  }
  if (body.type === 'model_call') {
    const knownFailure = body.state.type === 'terminal' && body.state.disposition === 'known_failed'
    const terminal = body.state.type === 'terminal'
    const terminalEvidenceIsCompatible =
      terminal ||
      (body.response == null && Object.values(body.usage).every((value) => value == null))
    return (
      kind === 'model_call_transition' &&
      terminalEvidenceIsCompatible &&
      (knownFailure
        ? body.provider_failure_cause != null && modelFailureCauses.has(body.provider_failure_cause)
        : body.provider_failure_cause == null)
    )
  }
  if (body.type === 'goal_event') {
    return kind === 'goal_turn_retired' && hasCompatibleGoalReason(body.event)
  }
  if (body.type === 'tool_batch') {
    return (
      kind === 'tool_batch_transition' &&
      body.goal_events.every(hasCompatibleGoalReason) &&
      body.tools.every((tool) => {
        const evidence = tool.evidence
        const evidenceMatches =
          evidence.type === 'request_only' ||
          (evidence.state === 'known_failed'
            ? typeof evidence.cause === 'string' && toolFailureCauses.has(evidence.cause)
            : evidence.cause == null)
        return (
          evidenceMatches &&
          tool.operator_required ===
            (tool.approval_judge_escalated || tool.approval_posture === 'human')
        )
      })
    )
  }
  if (body.type === 'reconciliation') {
    return (
      kind === 'turn_reconciliation_required' &&
      (body.operation.type === 'model_call' || body.operation.type === 'tool_attempt') &&
      positiveDecimal(body.attempt_count) &&
      body.cause_code === 'ambiguous_operation' &&
      body.exhausted &&
      body.operator_required
    )
  }
  if (body.type === 'tool_approval_decision') {
    return (
      kind === 'tool_approval_decided' &&
      (body.decision === 'approve' || body.decision === 'deny') &&
      (body.actor.type === 'policy' ||
        body.actor.type === 'user' ||
        body.actor.type === 'delegate') &&
      (body.actor.type !== 'delegate' || body.rationale != null)
    )
  }
  if (body.type === 'runner') {
    return kind === 'runner_state_transition' && positiveDecimal(body.placement_revision)
  }
  if (body.type === 'user_input') {
    return kind === 'input_accepted' && body.attachments.length === 0
  }
  return (compatibleKinds[body.type] as readonly DetailKind[]).includes(kind)
}

const modelCallState = (state: ModelCallBody['state']): string =>
  state.type === 'terminal' ? `terminal · ${state.disposition}` : state.type.replaceAll('_', ' ')

const modelSelection = (
  selection: { kind: 'direct'; selection_id: string } | { kind: 'alias'; alias_id: string },
): string =>
  selection.kind === 'direct'
    ? `direct · ${selection.selection_id}`
    : `alias · ${selection.alias_id}`

const effectiveSettingsFacts = (
  settings:
    | Extract<
        Extract<DetailBody, { type: 'model_settings' }>['detail'],
        { type: 'turn_resolved' }
      >['settings']
    | Extract<
        Extract<DetailBody, { type: 'model_settings' }>['detail'],
        { type: 'session_defaults_changed' }
      >['installed_settings'],
): ReadonlyArray<readonly [string, ReactNode]> => [
  ['Reasoning level', settings.effective.reasoning_level ?? 'default'],
  ['Fast mode', settings.effective.fast_mode],
  [
    'Service tier',
    settings.effective.service_tier
      ? `${settings.effective.service_tier.provider} · ${settings.effective.service_tier.value}`
      : 'default',
  ],
]

const TextDetail = ({ label, excerpt }: { label: string; excerpt: TextExcerpt }) => {
  return (
    <section className="session-detail-text" aria-label={label}>
      <header>
        <strong>{label}</strong>
        <span>
          offset {excerpt.offset_bytes} B · total {excerpt.total_bytes} B
        </span>
      </header>
      <pre>{excerpt.text}</pre>
    </section>
  )
}

const Facts = ({ facts }: { facts: ReadonlyArray<readonly [string, ReactNode]> }) => (
  <dl className="session-detail-facts">
    {facts.map(([label, value]) => (
      <div key={label}>
        <dt>{label}</dt>
        <dd>{value}</dd>
      </div>
    ))}
  </dl>
)

const GoalEventDetail = ({ event }: { event: GoalEvent }) => (
  <article className="session-detail-member">
    <Facts
      facts={[
        ['Goal event', event.type.replaceAll('_', ' ')],
        ['Generation', event.generation],
        ['Reason', event.type === 'blocked' ? event.reason : 'not recorded'],
      ]}
    />
    {'text' in event && event.text && <TextDetail label="Goal text" excerpt={event.text} />}
  </article>
)

const GoalEventList = ({ events }: { events: ReadonlyArray<GoalEvent> }): ReactNode => {
  const [event, ...remaining] = events
  if (!event) return null
  return (
    <>
      <GoalEventDetail event={event} />
      <GoalEventList events={remaining} />
    </>
  )
}

const ToolAttemptDetail = ({ tool }: { tool: ToolAttempt }) => {
  const evidence = tool.evidence
  const physical = evidence.type === 'physical_attempt' ? evidence : null
  return (
    <article className="session-detail-member">
      <h4>{tool.tool_name}</h4>
      <Facts
        facts={[
          ['Request', tool.request_id],
          ['Attempt', physical?.attempt_id ?? 'not issued'],
          ['State', physical?.state.replaceAll('_', ' ') ?? 'requested'],
          ['Approval', tool.approval_posture.replaceAll('_', ' ')],
          ['Effect', physical?.effect_posture.replaceAll('_', ' ') ?? 'not recorded'],
          ['Sandbox', physical?.sandbox_posture?.replaceAll('_', ' ') ?? 'not recorded'],
          ['Operator required', tool.operator_required ? 'yes' : 'no'],
          ['Judge escalated', tool.approval_judge_escalated ? 'yes' : 'no'],
          ['Cause', physical?.cause ?? 'not recorded'],
        ]}
      />
      {tool.arguments && <TextDetail label="Tool arguments" excerpt={tool.arguments} />}
      {physical?.result && <TextDetail label="Tool result" excerpt={physical.result} />}
      {physical?.failure && <TextDetail label="Tool failure" excerpt={physical.failure} />}
    </article>
  )
}

type Fact = readonly [string, ReactNode]

const delegationProvenanceFacts = (
  provenance: Extract<
    DelegationDetail,
    { type: 'child_lifecycle_disposition' | 'child_result' }
  >['provenance'],
): ReadonlyArray<Fact> => {
  switch (provenance.type) {
    case 'child_turn':
      return [
        ['Provenance', 'child turn'],
        ['Provenance session', provenance.session_id],
        ['Provenance turn', provenance.turn_id],
      ]
    case 'parent_turn_command':
      return [
        ['Provenance', 'parent turn command'],
        ['Provenance session', provenance.session_id],
        ['Provenance turn', provenance.turn_id],
        ['Provenance command', provenance.command_id],
      ]
    case 'parent_goal_command':
      return [
        ['Provenance', 'parent goal command'],
        ['Provenance session', provenance.session_id],
        ['Goal generation', provenance.goal_generation],
        ['Provenance command', provenance.command_id],
      ]
  }
}

const delegationFacts = (detail: DelegationDetail): ReadonlyArray<Fact> => {
  const common: ReadonlyArray<Fact> = [
    ['Event', detail.type.replaceAll('_', ' ')],
    ['Relationship', detail.relationship_id],
  ]
  switch (detail.type) {
    case 'child_spawned':
      return [
        ...common,
        ['Child session', detail.child_session_id],
        ['Policy', detail.policy.type],
        ...(detail.policy.type === 'bound'
          ? ([
              ['On parent stopped', detail.policy.on_parent_stopped.replaceAll('_', ' ')],
              ['On parent cancelled', detail.policy.on_parent_cancelled.replaceAll('_', ' ')],
            ] satisfies ReadonlyArray<Fact>)
          : []),
      ]
    case 'child_waiting':
      return [
        ...common,
        ['Child session', detail.child_session_id],
        ['Awaiting request', detail.awaiting_request_id],
        ['Wait mode', detail.mode],
      ]
    case 'child_lifecycle_disposition':
      return [
        ...common,
        ['Child session', detail.child_session_id],
        ['Event ordinal', detail.event_ordinal],
        ['Outcome', detail.outcome.replaceAll('_', ' ')],
        ['Reason', detail.reason.replaceAll('_', ' ')],
        ...delegationProvenanceFacts(detail.provenance),
      ]
    case 'child_result':
      return [
        ...common,
        ['Child session', detail.child_session_id],
        ['Outcome', detail.outcome.replaceAll('_', ' ')],
        ['Reason', detail.reason.replaceAll('_', ' ')],
        ...delegationProvenanceFacts(detail.provenance),
      ]
    case 'session_message':
      return [
        ...common,
        ['Message', detail.message_id],
        ['Sender session', detail.sender_session_id],
        ['Recipient session', detail.recipient_session_id],
        ['Message ordinal', detail.message_ordinal],
        ['Delivery sequence', detail.delivery_sequence],
      ]
    case 'result_wake':
      return [...common, ['Awaiting request', detail.awaiting_request_id ?? 'not recorded']]
    case 'message_wake':
      return [...common, ['Message', detail.message_id]]
  }
}

const unreachableBody = (body: never): never => {
  throw new TypeError(`unhandled generated timeline detail body: ${String(body)}`)
}

const detailContent = (body: DetailBody): ReactNode => {
  switch (body.type) {
    case 'session_created':
      return body.imported_evidence ? (
        <Facts
          facts={[
            ['Origin', 'imported'],
            ['Imported entry', body.imported_evidence.imported_entry_id],
            ['Imported position', body.imported_evidence.imported_position],
          ]}
        />
      ) : (
        <p className="session-detail-note">Native session creation.</p>
      )
    case 'model_settings':
      return (
        <Facts
          facts={
            body.detail.type === 'session_defaults_changed'
              ? [
                  ['Change', 'session defaults changed'],
                  ['Command', body.detail.command_id],
                  ['Prior version', body.detail.prior_defaults_version],
                  ['Installed version', body.detail.installed_defaults_version],
                  ['Prior model', modelSelection(body.detail.prior_model)],
                  ['Installed model', modelSelection(body.detail.installed_model)],
                  ...effectiveSettingsFacts(body.detail.installed_settings),
                ]
              : [
                  ['Change', 'turn settings resolved'],
                  ['Turn', body.detail.turn_id],
                  ['Accepted input', body.detail.accepted_input_id],
                  ['Defaults version', body.detail.defaults_version],
                  ['Requested model', modelSelection(body.detail.requested_model)],
                  ['Selected model', body.detail.selected_direct_id],
                  ...effectiveSettingsFacts(body.detail.settings),
                ]
          }
        />
      )
    case 'user_input': {
      const visibleAttachments = body.attachments.slice(0, MAX_RENDERED_ATTACHMENTS)
      return (
        <>
          <Facts facts={[['Turn', body.turn_id]]} />
          <TextDetail label="User input" excerpt={body.text} />
          {visibleAttachments.length > 0 && (
            <ul className="session-detail-attachments" aria-label="Attachment references">
              {visibleAttachments.map((attachment) => (
                <li key={attachment.blob_id}>
                  <code>{attachment.blob_id}</code>
                  <span>
                    {attachment.media_type ?? 'unknown media'} · {attachment.length_bytes} B
                  </span>
                </li>
              ))}
            </ul>
          )}
          {body.attachments.length > MAX_RENDERED_ATTACHMENTS && (
            <p className="session-detail-note">
              Showing {MAX_RENDERED_ATTACHMENTS} of {body.attachments.length} attachment references.
            </p>
          )}
        </>
      )
    }
    case 'model_call':
      return (
        <>
          <Facts
            facts={[
              ['Call', body.model_call_id],
              ['Turn', body.turn_id],
              ['Model', body.model_identity_id],
              ['Request context items', body.request_context_items],
              ['State', modelCallState(body.state)],
              ['Cause', body.provider_failure_cause ?? 'not recorded'],
              ['Input tokens', body.usage.input_tokens ?? 'not reported'],
              ['Output tokens', body.usage.output_tokens ?? 'not reported'],
              [
                'Cache creation input tokens',
                body.usage.cache_creation_input_tokens ?? 'not reported',
              ],
              ['Cache read input tokens', body.usage.cache_read_input_tokens ?? 'not reported'],
            ]}
          />
          {body.response ? (
            <TextDetail label="Model response" excerpt={body.response} />
          ) : (
            <p className="session-detail-note">No response text was recorded at this checkpoint.</p>
          )}
        </>
      )
    case 'tool_batch': {
      const tools = body.tools.slice(0, MAX_RENDERED_MEMBERS)
      const goalEvents = body.goal_events.slice(0, MAX_RENDERED_MEMBERS)
      return (
        <>
          <Facts
            facts={[
              ['Turn', body.turn_id],
              ['Producing call', body.producing_model_call_id],
              ['State', body.state.type.replaceAll('_', ' ')],
              [
                body.state.type === 'recovery_required' ? 'Recovery attempt' : 'Frontier',
                body.state.type === 'recovery_required'
                  ? body.state.tool_attempt_id
                  : body.state.frontier_id,
              ],
              ['Tool attempts in this page', String(body.tools.length)],
              ['Goal events in this page', String(body.goal_events.length)],
            ]}
          />
          {tools.length > 0 && (
            <section className="session-detail-members" aria-label="Tool attempts">
              {tools.map((tool) => (
                <ToolAttemptDetail key={tool.request_id} tool={tool} />
              ))}
            </section>
          )}
          {goalEvents.length > 0 && (
            <section className="session-detail-members" aria-label="Goal events">
              <GoalEventList events={goalEvents} />
            </section>
          )}
          {body.tools.length > tools.length && (
            <p className="session-detail-note">
              Showing {tools.length} of {body.tools.length} tool attempts.
            </p>
          )}
          {body.goal_events.length > goalEvents.length && (
            <p className="session-detail-note">
              Showing {goalEvents.length} of {body.goal_events.length} goal events.
            </p>
          )}
        </>
      )
    }
    case 'tool_approval_decision': {
      const actorFacts: ReadonlyArray<readonly [string, ReactNode]> =
        body.actor.type === 'policy'
          ? []
          : body.actor.type === 'user'
            ? [['Command', body.actor.command_id]]
            : [
                ['Model selection', body.actor.model_selection_id],
                ['Model call', body.actor.model_call_id],
              ]
      return (
        <>
          <Facts
            facts={[
              ['Tool', body.tool_name],
              ['Request', body.request_id],
              ['Turn', body.turn_id],
              ['Decision', body.decision.replaceAll('_', ' ')],
              ['Source', body.actor.type],
              ['Judge escalated', body.approval_judge_escalated ? 'yes' : 'no'],
              ...actorFacts,
            ]}
          />
          {body.rationale && <TextDetail label="Approval rationale" excerpt={body.rationale} />}
        </>
      )
    }
    case 'goal_event':
      return (
        <>
          <Facts facts={[['Turn', body.turn_id]]} />
          <GoalEventDetail event={body.event} />
        </>
      )
    case 'context_compaction':
      return (
        <>
          <Facts
            facts={[
              ['Compaction', body.compaction_id],
              ['Model call', body.model_call_id],
              ['Summary entry', body.summary_entry_id],
              ['Result frontier', body.result_frontier_id],
              ['Through position', body.through_position],
            ]}
          />
          <TextDetail label="Compaction summary" excerpt={body.summary} />
        </>
      )
    case 'turn_lifecycle':
      return (
        <Facts
          facts={[
            ['Turn', body.turn_id],
            ['Lifecycle', body.lifecycle],
            ['Cause', body.cause_code],
          ]}
        />
      )
    case 'reconciliation':
      return (
        <Facts
          facts={[
            ['Turn', body.turn_id],
            [
              'Operation',
              body.operation.type === 'model_call'
                ? body.operation.model_call_id
                : body.operation.tool_attempt_id,
            ],
            ['Kind', body.operation.type.replaceAll('_', ' ')],
            ['Attempts', body.attempt_count],
            ['Cause', body.cause_code],
            ['Exhausted', body.exhausted ? 'yes' : 'no'],
            ['Operator required', body.operator_required ? 'yes' : 'no'],
          ]}
        />
      )
    case 'runner':
      return (
        <Facts
          facts={[
            ['Runner', body.runner_id],
            ['State', body.state.replaceAll('_', ' ')],
            ['Placement revision', body.placement_revision],
            ['Sandbox', body.sandbox_posture.replaceAll('_', ' ')],
            ['Working directory', body.working_directory ?? 'not recorded'],
          ]}
        />
      )
    case 'delegation': {
      const detail = body.detail
      const content = 'content' in detail ? detail.content : null
      return (
        <>
          <Facts facts={delegationFacts(detail)} />
          {content && <TextDetail label="Delegation content" excerpt={content} />}
        </>
      )
    }
    default:
      return unreachableBody(body)
  }
}

const DetailRecord = ({ detail }: { detail: DetailItem }) => {
  return (
    <article className="session-detail-record">
      <div className="session-detail-record-heading">
        <strong>{detail.kind.replaceAll('_', ' ')}</strong>
        <span>{detail.projected_body_bytes} projected B</span>
      </div>
      {detailContent(detail.body)}
    </article>
  )
}

export function SessionItemDetail({
  source,
  sessionId,
  item,
}: {
  source: HttpSessionTimelineSource
  sessionId: string
  item: WebSessionTimelineWindow['items'][number]
}) {
  const [cursor, setCursor] = useState<NonNullable<WebSessionTimelineDetailPage['continuation']>>()
  const [restoreSummaryOnCompletion, setRestoreSummaryOnCompletion] = useState(false)
  const detail = useQuery({
    queryKey: ['production', 'session-item-detail', sessionId, item.address.event_sequence, cursor],
    queryFn: ({ signal }) =>
      source.readItemDetail(
        sessionId,
        item.address.event_sequence,
        { maxItems: DETAIL_PAGE_ITEMS, maxBytes: DETAIL_PAGE_BYTES },
        cursor,
        signal,
      ),
    gcTime: 0,
    placeholderData: (previous) => previous,
  })

  useEffect(() => {
    if (!restoreSummaryOnCompletion || detail.isFetching) return
    if (!detail.data && !detail.isError) return
    setRestoreSummaryOnCompletion(false)
    if (!detail.isError && detail.data?.continuation) return
    document
      .querySelector<HTMLButtonElement>(`[data-timeline-id="${item.address.event_sequence}"]`)
      ?.focus()
  }, [
    detail.data,
    detail.isError,
    detail.isFetching,
    item.address.event_sequence,
    restoreSummaryOnCompletion,
  ])

  if (detail.isError) {
    return (
      <p className="session-detail-state" role="alert">
        Detail unavailable: {detail.error.message}
      </p>
    )
  }
  if (!detail.data) return <p className="session-detail-state">Loading typed detail…</p>
  if (
    detail.data.items.some(
      (detailItem) =>
        detailItem.kind !== item.kind ||
        !isCompatibleDetailBody(detailItem.kind, detailItem.body, sessionId),
    )
  ) {
    return (
      <p className="session-detail-state" role="alert">
        Detail rejected because its event kind or body variant did not match the selected timeline
        header.
      </p>
    )
  }

  return (
    <div className="session-item-detail">
      {detail.data.items.map((detailItem) => (
        <DetailRecord
          key={`${detailItem.address.event_sequence}:${detailItem.body.type}`}
          detail={detailItem}
        />
      ))}
      {detail.data.items.length === 0 && (
        <p className="session-detail-note">No typed body was returned for this bounded page.</p>
      )}
      {detail.data.continuation && (
        <button
          type="button"
          className="session-detail-continue"
          aria-disabled={detail.isFetching}
          onClick={() => {
            if (!detail.isFetching) {
              setRestoreSummaryOnCompletion(true)
              setCursor(detail.data.continuation ?? undefined)
            }
          }}
        >
          {detail.isFetching
            ? 'Loading next bounded detail chunk…'
            : 'Load next bounded detail chunk'}
        </button>
      )}
    </div>
  )
}
