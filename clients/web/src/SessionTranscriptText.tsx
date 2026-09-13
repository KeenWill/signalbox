import { useQuery } from '@tanstack/react-query'
import { type RefObject, useRef, useState } from 'react'
import { AttachmentReferences } from './AttachmentReferences'
import { ToolCall } from './features/tools/ToolCall'
import type {
  WebSessionTimelineDetail,
  WebSessionTimelineDetailBody,
  WebTimelineDetailContinuation,
} from './generated/web-contract.mjs'
import { enumLabel } from './labels'
import {
  type HeldSessionTranscript,
  readExtendedSessionTranscript,
  type SessionTranscriptLimits,
} from './product'
import { conversationEntryKey } from './session-timeline/conversation'

function BodyText({
  body,
  preceding,
}: {
  body: WebSessionTimelineDetailBody
  preceding: readonly WebSessionTimelineDetail[]
}) {
  if (body.type === 'tool_batch')
    return (
      <>
        {body.tools.map((tool) => (
          <ToolCall
            key={tool.request_id}
            tool={tool}
            showMedia={
              !preceding.some(
                (item) =>
                  item.body.type === 'tool_batch' &&
                  item.body.tools.some(
                    (prior) =>
                      prior.request_id === tool.request_id &&
                      prior.evidence.type === 'physical_attempt' &&
                      prior.evidence.result_media_reference != null,
                  ),
              )
            }
          />
        ))}
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
      <span className="eyebrow">{body.type === 'user_input' ? 'Accepted input' : 'Assistant'}</span>
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

interface SessionTranscriptTextProps {
  sessionId: string
  first: string
  through: string
  observed: string
  limits: SessionTranscriptLimits
}

export function SessionTranscriptText(props: SessionTranscriptTextProps) {
  const held = useRef<HeldSessionTranscript | null>(null)
  return (
    <TranscriptWindow
      key={`${props.sessionId}:${props.first}:${props.through}:${props.observed}`}
      {...props}
      held={held}
    />
  )
}

function TranscriptWindow({
  sessionId,
  first,
  through,
  observed,
  limits,
  held,
}: SessionTranscriptTextProps & { held: RefObject<HeldSessionTranscript | null> }) {
  const [continuation, setContinuation] = useState<WebTimelineDetailContinuation | null>(null)
  const transcript = useQuery({
    queryKey: [
      'production',
      'session-text',
      sessionId,
      first,
      through,
      observed,
      limits,
      continuation,
    ],
    queryFn: async ({ signal }) => {
      const next = await readExtendedSessionTranscript(
        { sessionId, first, through },
        continuation,
        limits,
        held.current,
        signal,
      )
      if (!signal.aborted) held.current = next
      return { ...next.page, hasEarlierItems: next.omittedThrough !== null }
    },
    gcTime: 0,
  })
  return (
    <section className="session-transcript-text" aria-label="Transcript text">
      {transcript.isPending && <p>Loading transcript…</p>}
      {transcript.isError && (
        <p role="alert">
          Transcript failed to load.{' '}
          <button type="button" onClick={() => void transcript.refetch()}>
            Retry transcript text
          </button>
        </p>
      )}
      {transcript.data?.items.map((item, index) => (
        <div
          key={conversationEntryKey(item)}
          className="session-message-entry"
          data-event-sequence={item.address.event_sequence}
        >
          <BodyText body={item.body} preceding={transcript.data?.items.slice(0, index) ?? []} />
        </div>
      ))}
      <div className="session-text-pagination">
        {(continuation !== null || transcript.data?.hasEarlierItems) && (
          <button
            type="button"
            onClick={() => {
              held.current = null
              if (continuation !== null) setContinuation(null)
              else void transcript.refetch()
            }}
          >
            First text page
          </button>
        )}
        {transcript.data?.continuation && (
          <button
            type="button"
            onClick={() => setContinuation(transcript.data?.continuation ?? null)}
          >
            Next text page
          </button>
        )}
      </div>
    </section>
  )
}
