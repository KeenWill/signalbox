import { useInfiniteQuery } from '@tanstack/react-query'
import { type ReactNode, useEffect, useMemo, useRef, useState } from 'react'
import { AttachmentReferences } from './AttachmentReferences'
import type { CommandContext } from './commands'
import type {
  WebSessionTimelineDetailBody,
  WebTimelineTextExcerpt,
  WebTimelineToolAttempt,
} from './generated/web-contract.mjs'
import { enumLabel } from './labels'
import type { SessionTranscriptLimits } from './product'
import { conversationEntryKey } from './session-timeline/conversation'
import type { SessionWindowAnchor } from './session-timeline/model'
import { readTranscriptWindow, TRANSCRIPT_RETAINED_WINDOWS } from './session-timeline/transcript'
import { groupTranscriptTurns, type TranscriptTurn } from './session-timeline/turns'
import type { DetailMode } from './state'
import { VirtualTranscript } from './Transcript'

function ToolText({ label, excerpt }: { label: string; excerpt: WebTimelineTextExcerpt }) {
  let content = excerpt.text
  try {
    content = JSON.stringify(JSON.parse(excerpt.text), null, 2)
  } catch {
    // Plain text and partial JSON remain readable as supplied.
  }
  return (
    <section className="session-tool-text" aria-label={label}>
      <strong>{label}</strong>
      <pre>{content}</pre>
      {(excerpt.offset_bytes !== '0' || excerpt.continuation != null) && (
        <small>
          From byte {excerpt.offset_bytes} of {excerpt.total_bytes}
        </small>
      )}
    </section>
  )
}

function BodyText({ body }: { body: WebSessionTimelineDetailBody }) {
  if (body.type === 'tool_batch')
    return (
      <>
        {body.tools.map((tool) => {
          const physical = tool.evidence.type === 'physical_attempt' ? tool.evidence : null
          return (
            <div key={tool.request_id} className="session-tool-entry">
              <strong>{tool.tool_name}</strong>
              {tool.arguments && <ToolText label="Arguments" excerpt={tool.arguments} />}
              {physical?.result && <ToolText label="Output" excerpt={physical.result} />}
              {physical?.failure && <ToolText label="Failure" excerpt={physical.failure} />}
            </div>
          )
        })}
      </>
    )
  if (body.type === 'reconciliation')
    return (
      <p className="session-turn-outcome">Turn needs recovery · {enumLabel(body.operation.type)}</p>
    )
  if (body.type === 'turn_lifecycle')
    return <p className="session-turn-outcome">{enumLabel(`turn_${body.cause_code}`)}</p>
  if (body.type === 'event_fact' && body.kind === 'goal_turn_retired')
    return <p className="session-turn-outcome">{enumLabel(body.kind)}</p>
  if (body.type === 'event_fact' && body.kind === 'automatic_reconciliation_exhausted')
    return <p className="session-turn-outcome">Automatic reconciliation exhausted.</p>
  const excerpt =
    body.type === 'user_input' ? body.text : body.type === 'model_call' ? body.response : null
  if (!excerpt)
    return body.type === 'model_call' && body.provider_failure_cause ? (
      <p className="session-turn-outcome">
        Provider error: {enumLabel(body.provider_failure_cause)}
      </p>
    ) : null
  return (
    <>
      <span className="eyebrow">{body.type === 'user_input' ? 'You' : 'Assistant'}</span>
      <p className="session-message-text">{excerpt.text}</p>
      {body.type === 'user_input' && <AttachmentReferences attachments={body.attachments} />}
      {(excerpt.offset_bytes !== '0' || excerpt.continuation !== null) && (
        <small>
          From byte {excerpt.offset_bytes} of {excerpt.total_bytes}
        </small>
      )}
    </>
  )
}

export interface SessionTranscriptTextProps {
  sessionId: string
  first: string
  through: string
  observed: string
  limits: SessionTranscriptLimits
  eventSequence?: string
  turnId?: string
  context?: CommandContext
  renderTool?: (tool: WebTimelineToolAttempt, detail: DetailMode) => ReactNode
}

export function SessionTranscriptText(props: SessionTranscriptTextProps) {
  return (
    <TranscriptWindow
      key={`${props.sessionId}:${props.eventSequence ?? ''}:${props.turnId ?? ''}`}
      {...props}
    />
  )
}

function TranscriptWindow({
  sessionId,
  observed,
  limits,
  eventSequence,
  renderTool,
}: SessionTranscriptTextProps) {
  const transcript = useInfiniteQuery({
    queryKey: ['production', 'scrolling-transcript', sessionId, eventSequence, limits],
    initialPageParam: (eventSequence
      ? { kind: 'around', eventSequence }
      : { kind: 'latest' }) as SessionWindowAnchor,
    queryFn: ({ pageParam, signal }) => readTranscriptWindow(sessionId, pageParam, limits, signal),
    getPreviousPageParam: (page): SessionWindowAnchor | undefined =>
      page.window.continuation_before
        ? { kind: 'before', eventSequence: page.window.continuation_before.event_sequence }
        : undefined,
    getNextPageParam: (page): SessionWindowAnchor | undefined =>
      page.window.continuation_after
        ? { kind: 'after', eventSequence: page.window.continuation_after.event_sequence }
        : undefined,
    maxPages: TRANSCRIPT_RETAINED_WINDOWS,
    gcTime: 0,
  })
  useEffect(() => {
    if (transcript.error) console.error('Transcript load failed', transcript.error)
  }, [transcript.error])
  const previousObservation = useRef(observed)
  useEffect(() => {
    if (previousObservation.current !== observed) {
      previousObservation.current = observed
      void transcript.refetch()
    }
  }, [observed, transcript.refetch])
  const pages = transcript.data?.pages
  const entries = useMemo(
    () => pages?.flatMap((page) => page.details.flatMap((detail) => detail.items)) ?? [],
    [pages],
  )
  const turns = useMemo(
    () =>
      groupTranscriptTurns(entries).filter(
        (turn) => turn.messages.length > 0 || turn.result || turn.tools.length > 0,
      ),
    [entries],
  )
  const ids = useMemo(() => turns.map((turn) => turn.id), [turns])
  return (
    <section className="session-transcript-text" aria-label="Transcript text">
      {transcript.isPending && <p role="status">Loading transcript…</p>}
      {transcript.isError && (
        <p role="alert">
          Transcript failed to load.{' '}
          <button type="button" onClick={() => void transcript.refetch()}>
            Retry transcript
          </button>
        </p>
      )}
      <VirtualTranscript
        ids={ids}
        initialEnd={!eventSequence}
        selectedId={
          turns.find((turn) =>
            turn.events.some((event) => event.address.event_sequence === eventSequence),
          )?.id
        }
        onEdge={(direction) => {
          if (transcript.isFetching || transcript.isError) return
          if (direction === 'before' && transcript.hasPreviousPage)
            void transcript.fetchPreviousPage()
          if (direction === 'after' && transcript.hasNextPage) void transcript.fetchNextPage()
        }}
        renderRow={(index, measure, style) => {
          const turn = turns[index]
          if (!turn) return null
          return (
            <div
              key={turn.id}
              ref={measure}
              data-index={index}
              style={style}
              className="session-message-entry session-turn"
              data-turn-id={turn.turnId}
            >
              <TurnSummary turn={turn} renderTool={renderTool} />
            </div>
          )
        }}
      />
      {transcript.isFetching && !transcript.isPending && (
        <small role="status">Loading messages…</small>
      )}
    </section>
  )
}

function TurnSummary({
  turn,
  renderTool,
}: {
  turn: TranscriptTurn
  renderTool?: SessionTranscriptTextProps['renderTool']
}) {
  const [openTool, setOpenTool] = useState<string | null>(null)
  const tool = turn.tools.find((entry) => entry.request_id === openTool)
  return (
    <>
      {turn.messages.map((item) => (
        <div key={conversationEntryKey(item)} data-event-sequence={item.address.event_sequence}>
          <BodyText body={item.body} />
        </div>
      ))}
      {turn.tools.length > 0 && (
        <section className="session-tool-chips" aria-label="Tools used">
          {turn.tools.map((entry) => (
            <button
              type="button"
              key={entry.request_id}
              aria-expanded={openTool === entry.request_id}
              onClick={() => setOpenTool(openTool === entry.request_id ? null : entry.request_id)}
            >
              {entry.tool_name}
            </button>
          ))}
        </section>
      )}
      {tool && (
        <div className="session-tool-slot">
          {renderTool ? renderTool(tool, 'condensed') : <ToolSummary tool={tool} />}
        </div>
      )}
      {turn.result && (
        <div data-event-sequence={turn.result.address.event_sequence}>
          <BodyText body={turn.result.body} />
        </div>
      )}
    </>
  )
}

function ToolSummary({ tool }: { tool: WebTimelineToolAttempt }) {
  const evidence = tool.evidence.type === 'physical_attempt' ? tool.evidence : null
  return (
    <section aria-label={`${tool.tool_name} details`}>
      <strong>{tool.tool_name}</strong>
      {tool.arguments && <p className="session-tool-summary">{tool.arguments.text}</p>}
      {evidence?.result && <p className="session-tool-summary">{evidence.result.text}</p>}
      {evidence?.failure && <p className="session-tool-summary">{evidence.failure.text}</p>}
    </section>
  )
}
