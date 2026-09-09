import { useQuery } from '@tanstack/react-query'
import { type ReactNode, useEffect, useRef, useState } from 'react'
import { AttachmentReferences } from './AttachmentReferences'
import type {
  WebSessionTimelineDetailPage,
  WebSessionTimelineWindow,
} from './generated/web-contract.mjs'
import { enumLabel } from './labels'
import { readSessionTranscript, type SessionTranscriptLimits } from './product'

type DetailItem = WebSessionTimelineDetailPage['items'][number]
type DetailBody = DetailItem['body']
type TextExcerpt = Extract<DetailBody, { type: 'user_input' }>['text']
type GoalEvent = Extract<DetailBody, { type: 'goal_event' }>['event']
type ToolAttempt = Extract<DetailBody, { type: 'tool_batch' }>['tools'][number]
type DelegationDetail = Extract<DetailBody, { type: 'delegation' }>['detail']

const compatibleKinds = {
  session_created: ['session_created'],
  session_state: ['session_state_changed'],
  session_terminal: ['session_terminal'],
  command_settlement: ['command_settled'],
  injection_settlement: ['injection_settled'],
  ownership: ['session_ownership_changed'],
  model_settings: ['session_model_settings_changed', 'turn_model_settings_resolved'],
  user_input: ['input_accepted'],
  model_call: ['model_call_transition'],
  tool_batch: ['tool_batch_transition'],
  tool_approval_decision: ['tool_approval_decided'],
  goal_event: ['goal_changed'],
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
  event_fact: ['goal_turn_retired'],
} as const satisfies Record<DetailBody['type'], readonly DetailItem['kind'][]>

export const isCompatibleDetailBody = (kind: DetailItem['kind'], body: DetailBody): boolean => {
  const kinds: readonly string[] | undefined = compatibleKinds[body.type]
  if (!Array.isArray(kinds) || !kinds.includes(kind)) return false
  if (body.type === 'event_fact') return body.kind === kind
  if (body.type === 'model_settings')
    return kind === 'session_model_settings_changed'
      ? body.detail.type === 'session_defaults_changed'
      : body.detail.type === 'turn_resolved'
  if (body.type === 'turn_lifecycle')
    return kind === 'turn_activated'
      ? body.lifecycle === 'activated' && body.cause_code === 'activated'
      : body.lifecycle === 'terminalized' && body.cause_code === kind.slice('turn_'.length)
  if (body.type === 'delegation')
    return body.detail.type === 'message_wake' || body.detail.type === 'result_wake'
      ? kind === 'delegation_wake'
      : kind === 'delegation_update'
  return true
}

const modelCallState = (state: Extract<DetailBody, { type: 'model_call' }>['state']): string =>
  state.type === 'terminal' ? `Finished · ${enumLabel(state.disposition)}` : enumLabel(state.type)

const modelSelection = (
  selection: { kind: 'direct'; selection_id: string } | { kind: 'alias'; alias_id: string },
): string =>
  selection.kind === 'direct'
    ? `Direct · ${selection.selection_id}`
    : `Alias · ${selection.alias_id}`

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
  ['Reasoning level', enumLabel(settings.effective.reasoning_level ?? 'Default')],
  ['Fast mode', enumLabel(settings.effective.fast_mode)],
  [
    'Service tier',
    settings.effective.service_tier
      ? `${enumLabel(settings.effective.service_tier.provider)} · ${enumLabel(settings.effective.service_tier.value)}`
      : 'Default',
  ],
]

const boundedSettingEvidence = (value: unknown): ReactNode => <code>{JSON.stringify(value)}</code>

const TextDetail = ({ label, excerpt }: { label: string; excerpt: TextExcerpt }) => {
  return (
    <section className="session-detail-text" aria-label={label}>
      <header>
        <strong>{label}</strong>
        <span>
          From byte {excerpt.offset_bytes} of {excerpt.total_bytes}
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
        ['Goal event', enumLabel(event.type)],
        ['Generation', event.generation],
        [
          'Reason',
          event.type === 'blocked'
            ? enumLabel(event.reason)
            : event.type === 'session_closed'
              ? enumLabel(event.outcome)
              : 'Not recorded',
        ],
      ]}
    />
    {event.type === 'user_stopped' && (
      <Facts
        facts={[
          ['Closing turn', event.settling_turn_id ?? 'None'],
          ['Abandoned approved actions', event.abandoned_actions ?? 'Pending'],
        ]}
      />
    )}
    {'text' in event && event.text && <TextDetail label="Goal text" excerpt={event.text} />}
  </article>
)

const ToolAttemptDetail = ({ tool }: { tool: ToolAttempt }) => {
  const evidence = tool.evidence
  const physical = evidence.type === 'physical_attempt' ? evidence : null
  return (
    <article className="session-detail-member">
      <h4>{tool.tool_name}</h4>
      <Facts
        facts={[
          ['Request', tool.request_id],
          ['Attempt', physical?.attempt_id ?? 'Not recorded'],
          ['State', enumLabel(physical?.state ?? 'Requested')],
          ['Approval', enumLabel(tool.approval_posture)],
          ['Effect', enumLabel(physical?.effect_posture ?? 'Not recorded')],
          ['Sandbox', enumLabel(physical?.sandbox_posture ?? 'Not recorded')],
          ['Judge escalated to user', tool.approval_judge_escalated ? 'Yes' : 'No'],
          ['Cause', enumLabel(physical?.cause ?? 'Not recorded')],
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
        ['Source', 'Child turn'],
        ['Source session', provenance.session_id],
        ['Source turn', provenance.turn_id],
      ]
    case 'parent_turn_command':
      return [
        ['Source', 'Parent turn command'],
        ['Source session', provenance.session_id],
        ['Source turn', provenance.turn_id],
        ['Source command', provenance.command_id],
      ]
    case 'parent_lifecycle_command':
      return [
        ['Source', 'Parent lifecycle command'],
        ['Session', provenance.session_id],
        ['Command', provenance.command_id],
      ]
    case 'parent_goal_command':
      return [
        ['Source', 'Parent goal command'],
        ['Source session', provenance.session_id],
        ['Goal generation', provenance.goal_generation],
        ['Source command', provenance.command_id],
      ]
  }
}

const delegationFacts = (detail: DelegationDetail): ReadonlyArray<Fact> => {
  const common: ReadonlyArray<Fact> = [
    ['Event', enumLabel(detail.type)],
    ['Relationship', detail.relationship_id],
  ]
  switch (detail.type) {
    case 'child_spawned':
      return [
        ...common,
        ['Child session', detail.child_session_id],
        ['Policy', enumLabel(detail.policy.type)],
        ...(detail.policy.type === 'bound'
          ? ([
              ['On parent stopped', enumLabel(detail.policy.on_parent_stopped)],
              ['On parent cancelled', enumLabel(detail.policy.on_parent_cancelled)],
            ] satisfies ReadonlyArray<Fact>)
          : []),
      ]
    case 'child_waiting':
      return [
        ...common,
        ['Child session', detail.child_session_id],
        ['Awaiting request', detail.awaiting_request_id],
        ['Wait mode', enumLabel(detail.mode)],
      ]
    case 'child_lifecycle_disposition':
      return [
        ...common,
        ['Child session', detail.child_session_id],
        ['Event number', detail.event_ordinal],
        ['Outcome', enumLabel(detail.outcome)],
        ['Reason', enumLabel(detail.reason)],
        ...delegationProvenanceFacts(detail.provenance),
      ]
    case 'child_result':
      return [
        ...common,
        ['Child session', detail.child_session_id],
        ['Outcome', enumLabel(detail.outcome)],
        ['Reason', enumLabel(detail.reason)],
        ...delegationProvenanceFacts(detail.provenance),
      ]
    case 'session_message':
      return [
        ...common,
        ['Message', detail.message_id],
        ['Sender session', detail.sender_session_id],
        ['Recipient session', detail.recipient_session_id],
        ['Message number', detail.message_ordinal],
        ['Delivery order', detail.delivery_sequence],
      ]
    case 'result_wake':
      return [...common, ['Awaiting request', detail.awaiting_request_id ?? 'Not recorded']]
    case 'message_wake':
      return [...common, ['Message', detail.message_id]]
  }
}

const unreachableBody = (body: never): never => {
  throw new TypeError(`unhandled generated timeline detail body: ${String(body)}`)
}

const detailContent = (body: DetailBody): ReactNode => {
  switch (body.type) {
    case 'session_state':
      return <Facts facts={[['State', enumLabel(body.state)]]} />
    case 'session_terminal':
      return <Facts facts={[['Outcome', enumLabel(body.outcome)]]} />
    case 'command_settlement':
      return (
        <Facts
          facts={[
            ['Command', body.command_id],
            ['Result', body.rejection ?? 'Applied'],
          ]}
        />
      )
    case 'injection_settlement':
      return (
        <Facts
          facts={[
            ['Command', body.command_id],
            ['Delivery', body.delivered ? 'Delivered' : 'Not delivered'],
            ['Turn', body.turn_id ?? 'None'],
            ['Rejection', body.rejection ?? 'None'],
          ]}
        />
      )
    case 'ownership':
      return <Facts facts={[['Ownership', enumLabel(body.transition)]]} />
    case 'event_fact':
      return <p>Turn retired before it started.</p>

    case 'session_created':
      return (
        <Facts
          facts={[
            ['Cause', enumLabel(body.cause.type)],
            ...(body.cause.type === 'delegated'
              ? ([['Created by request', body.cause.spawning_request_id]] as const)
              : body.cause.type === 'workflow'
                ? ([['Program run', body.cause.program_run_id]] as const)
                : body.cause.type === 'interactive'
                  ? []
                  : ([['Dispatch', body.cause.dispatch_id]] as const)),
            ...(body.imported_evidence
              ? ([
                  ['Origin', 'Imported'],
                  ['Imported conversation', body.imported_evidence.imported_conversation_id],
                  ['Relationship', enumLabel(body.imported_evidence.relationship)],
                  ['Imported entry', body.imported_evidence.imported_entry_id],
                  ['Imported position', body.imported_evidence.imported_position],
                ] as const)
              : []),
          ]}
        />
      )
    case 'model_settings':
      return (
        <Facts
          facts={
            body.detail.type === 'session_defaults_changed'
              ? [
                  ['Change', 'Session defaults changed'],
                  ['Command', body.detail.command_id],
                  ['Prior version', body.detail.prior_defaults_version],
                  ['Installed version', body.detail.installed_defaults_version],
                  ['Prior model', modelSelection(body.detail.prior_model)],
                  ['Installed model', modelSelection(body.detail.installed_model)],
                  ['Override', boundedSettingEvidence(body.detail.caller_override)],
                  ['Prior settings', boundedSettingEvidence(body.detail.prior_settings)],
                  [
                    'Setting precedence',
                    boundedSettingEvidence(body.detail.installed_settings.precedence),
                  ],
                  ['Adjustments', boundedSettingEvidence(body.detail.adjustments)],
                  ...effectiveSettingsFacts(body.detail.installed_settings),
                ]
              : [
                  ['Change', 'Turn settings selected'],
                  ['Turn', body.detail.turn_id],
                  ['Accepted input', body.detail.accepted_input_id],
                  ['Defaults version', body.detail.defaults_version],
                  ['Requested model', modelSelection(body.detail.requested_model)],
                  ['Selected model', body.detail.selected_direct_id],
                  [
                    'Adjusted from selection',
                    body.detail.adjusted_from_selection_id ?? 'Not adjusted',
                  ],
                  ['Per-call override', boundedSettingEvidence(body.detail.per_call_override)],
                  ['Settings precedence', boundedSettingEvidence(body.detail.settings.precedence)],
                  ['Adjustments', boundedSettingEvidence(body.detail.adjustments)],
                  ...effectiveSettingsFacts(body.detail.settings),
                ]
          }
        />
      )
    case 'user_input': {
      return (
        <>
          <Facts facts={[['Turn', body.turn_id]]} />
          <TextDetail label="Accepted input" excerpt={body.text} />
          <AttachmentReferences attachments={body.attachments} />
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
              ['Cause', enumLabel(body.provider_failure_cause ?? 'Not recorded')],
              ['Input tokens', body.usage.input_tokens ?? 'Not reported'],
              ['Output tokens', body.usage.output_tokens ?? 'Not reported'],
              [
                'Cache creation input tokens',
                body.usage.cache_creation_input_tokens ?? 'Not reported',
              ],
              ['Cache read input tokens', body.usage.cache_read_input_tokens ?? 'Not reported'],
            ]}
          />
          {body.response ? (
            <TextDetail label="Model response" excerpt={body.response} />
          ) : (
            <p className="session-detail-note">No response text.</p>
          )}
        </>
      )
    case 'tool_batch': {
      const tools = body.tools
      const goalEvents = body.goal_events
      return (
        <>
          <Facts
            facts={[
              ['Turn', body.turn_id],
              ['Model call', body.producing_model_call_id],
              ['State', enumLabel(body.state.type)],
              [
                body.state.type === 'recovery_required' ? 'Recovery attempt' : 'Frontier ID',
                body.state.type === 'recovery_required'
                  ? body.state.tool_attempt_id
                  : body.state.frontier_id,
              ],
              ['Item number', body.projected_member_index ?? 'None'],
              ['Tool requests', String(body.tools.length)],
              ['Goal events', String(body.goal_events.length)],
            ]}
          />
          {tools.length > 0 && (
            <section className="session-detail-members" aria-label="Tool requests">
              {tools.map((tool) => (
                <ToolAttemptDetail key={tool.request_id} tool={tool} />
              ))}
            </section>
          )}
          {goalEvents.length > 0 && (
            <section className="session-detail-members" aria-label="Goal events">
              {goalEvents[0] && <GoalEventDetail event={goalEvents[0]} />}
            </section>
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
            : body.actor.type === 'user_override'
              ? [
                  ['Command', body.actor.command_id],
                  ['Denied request', body.actor.denied_request_id],
                ]
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
              ['Decision', enumLabel(body.decision)],
              ['Source', enumLabel(body.actor.type)],
              ['Judge escalated to user', body.approval_judge_escalated ? 'Yes' : 'No'],
              ...actorFacts,
            ]}
          />
          {body.rationale && <TextDetail label="Approval rationale" excerpt={body.rationale} />}
        </>
      )
    }
    case 'goal_event':
      return <GoalEventDetail event={body.event} />
    case 'context_compaction':
      return (
        <>
          <Facts
            facts={[
              ['Compaction', body.compaction_id],
              ['Model call', body.model_call_id],
              ['Summary entry', body.summary_entry_id],
              ['Result ID', body.result_frontier_id],
              ['Up to position', body.through_position],
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
            ['Status', enumLabel(body.lifecycle)],
            ['Cause', enumLabel(body.cause_code)],
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
            ['Kind', enumLabel(body.operation.type)],
            ['Final ID', body.terminal_frontier_id],
          ]}
        />
      )
    case 'runner':
      return (
        <Facts
          facts={[
            ['Runner', body.runner_id],
            ['State', enumLabel(body.state)],
            ['Placement revision', body.placement_revision],
            ['Sandbox', enumLabel(body.sandbox_posture)],
            ['Working directory', body.working_directory ?? 'Not recorded'],
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

export function SessionItemDetail({
  sessionId,
  item,
  limits,
  onComplete,
}: {
  sessionId: string
  item: WebSessionTimelineWindow['items'][number]
  limits: SessionTranscriptLimits
  onComplete: () => void
}) {
  const [cursor, setCursor] = useState<WebSessionTimelineDetailPage['continuation']>(null)
  const previous = useRef<WebSessionTimelineDetailPage | undefined>(undefined)
  const detail = useQuery({
    queryKey: [
      'production',
      'session-item-detail',
      sessionId,
      item.address.event_sequence,
      cursor,
      limits,
    ],
    queryFn: ({ signal }) =>
      readSessionTranscript(
        sessionId,
        item.address.event_sequence,
        item.address.event_sequence,
        cursor ?? null,
        limits,
        signal,
        previous.current,
      ),
    gcTime: 0,
  })
  useEffect(() => {
    if (detail.error) console.error('Detail load failed', detail.error)
  }, [detail.error])
  const record = detail.data?.items[0]
  const compatible =
    record && record.kind === item.kind && isCompatibleDetailBody(record.kind, record.body)
  const continuation = compatible ? detail.data?.continuation : null
  let content: ReactNode
  if (detail.isError) content = <p role="alert">Details couldn't be loaded.</p>
  else if (!detail.data) content = <p role="status">Loading…</p>
  else if (!compatible) content = <p role="alert">Details didn't match this event.</p>
  else content = detailContent(record.body)
  return (
    <article aria-label={`${enumLabel(item.kind)} detail`}>
      {content}
      {continuation || cursor ? (
        <button
          type="button"
          aria-disabled={detail.isPending || undefined}
          onClick={(event) => {
            event.stopPropagation()
            if (detail.isPending) return
            if (continuation) {
              previous.current = detail.data
              setCursor(continuation)
            } else onComplete()
          }}
        >
          {detail.isPending || continuation ? 'Load more' : 'Return to event'}
        </button>
      ) : null}
    </article>
  )
}
