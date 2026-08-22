import { useQuery } from '@tanstack/react-query'
import { type ReactNode, useState } from 'react'
import type {
  WebSessionTimelineDetailPage,
  WebSessionTimelineWindow,
} from './generated/web-contract.mjs'
import type { HttpSessionTimelineSource } from './session-timeline/model'

const DETAIL_PAGE_ITEMS = 1
const DETAIL_PAGE_BYTES = 16 * 1024
const MAX_RENDERED_ATTACHMENTS = 24

type DetailItem = WebSessionTimelineDetailPage['items'][number]
type DetailBody = DetailItem['body']
type TextExcerpt = Extract<DetailBody, { type: 'user_input' }>['text']
type ModelCallBody = Extract<DetailBody, { type: 'model_call' }>
type GoalEvent = Extract<DetailBody, { type: 'goal_event' }>['event']
type ToolAttempt = Extract<DetailBody, { type: 'tool_batch' }>['tools'][number]

const MAX_RENDERED_MEMBERS = 24

const modelCallState = (state: ModelCallBody['state']): string =>
  state.type === 'terminal' ? `terminal · ${state.disposition}` : state.type.replaceAll('_', ' ')

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
        ['Goal event', event.event_kind.replaceAll('_', ' ')],
        ['Generation', event.generation],
        ['Reason', event.reason ?? 'not recorded'],
      ]}
    />
    {event.text && <TextDetail label="Goal text" excerpt={event.text} />}
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

const ToolAttemptDetail = ({ tool }: { tool: ToolAttempt }) => (
  <article className="session-detail-member">
    <h4>{tool.tool_name}</h4>
    <Facts
      facts={[
        ['Request', tool.request_id],
        ['Attempt', tool.attempt_id ?? 'not issued'],
        ['State', tool.state?.replaceAll('_', ' ') ?? 'requested'],
        ['Approval', tool.approval_posture.replaceAll('_', ' ')],
        ['Effect', tool.effect_posture?.replaceAll('_', ' ') ?? 'not recorded'],
        ['Sandbox', tool.sandbox_posture?.replaceAll('_', ' ') ?? 'not recorded'],
        ['Operator required', tool.operator_required ? 'yes' : 'no'],
        ['Judge escalated', tool.approval_judge_escalated ? 'yes' : 'no'],
        ['Cause', tool.cause_code ?? 'not recorded'],
      ]}
    />
    {tool.arguments && <TextDetail label="Tool arguments" excerpt={tool.arguments} />}
    {tool.result && <TextDetail label="Tool result" excerpt={tool.result} />}
    {tool.failure && <TextDetail label="Tool failure" excerpt={tool.failure} />}
  </article>
)

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
          facts={[
            ['Cause', body.cause_code],
            ['Turn', body.turn_id ?? 'session default'],
          ]}
        />
      )
    case 'user_input': {
      const visibleAttachments = body.attachments.slice(0, MAX_RENDERED_ATTACHMENTS)
      return (
        <>
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
              ['Model', body.model_identity_id],
              ['State', modelCallState(body.state)],
              ['Input tokens', body.usage.input_tokens ?? 'not reported'],
              ['Output tokens', body.usage.output_tokens ?? 'not reported'],
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
              ['State', body.state.replaceAll('_', ' ')],
              ['Tools', String(body.tools.length)],
              ['Goal events', String(body.goal_events.length)],
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
    case 'tool_approval_decision':
      return (
        <>
          <Facts
            facts={[
              ['Tool', body.tool_name],
              ['Request', body.request_id],
              ['Turn', body.turn_id],
              ['Decision', body.decision.replaceAll('_', ' ')],
              ['Source', body.source],
              ['Judge escalated', body.approval_judge_escalated ? 'yes' : 'no'],
            ]}
          />
          {body.rationale && <TextDetail label="Approval rationale" excerpt={body.rationale} />}
        </>
      )
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
            ['Operation', body.operation_id],
            ['Kind', body.operation_kind.replaceAll('_', ' ')],
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
    case 'delegation':
      return (
        <>
          <Facts
            facts={[
              ['Event', body.event_kind.replaceAll('_', ' ')],
              ['Relationship', body.relationship_id],
              ['Subject', body.subject_id ?? 'not recorded'],
              ['Outcome', body.outcome?.replaceAll('_', ' ') ?? 'not recorded'],
              ['Reason', body.reason ?? 'not recorded'],
            ]}
          />
          {body.content && <TextDetail label="Delegation content" excerpt={body.content} />}
        </>
      )
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
  })

  if (detail.isError) {
    return (
      <p className="session-detail-state" role="alert">
        Detail unavailable: {detail.error.message}
      </p>
    )
  }
  if (!detail.data) return <p className="session-detail-state">Loading typed detail…</p>
  if (detail.data.items.some((detailItem) => detailItem.kind !== item.kind)) {
    return (
      <p className="session-detail-state" role="alert">
        Detail rejected because its event kind did not match the selected timeline header.
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
          onClick={() => setCursor(detail.data.continuation ?? undefined)}
        >
          Load next bounded detail chunk
        </button>
      )}
    </div>
  )
}
