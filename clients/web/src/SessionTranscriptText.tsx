import { useQuery } from '@tanstack/react-query'
import { type RefObject, useRef, useState } from 'react'
import { AttachmentReferences } from './AttachmentReferences'
import type {
  WebSessionTimelineDetailBody,
  WebTimelineDetailContinuation,
  WebTimelineTextExcerpt,
} from './generated/web-contract.mjs'
import { enumLabel } from './labels'
import {
  type HeldSessionTranscript,
  readExtendedSessionTranscript,
  type SessionTranscriptLimits,
} from './product'
import { conversationEntryKey } from './session-timeline/conversation'

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
          Showing from byte {excerpt.offset_bytes} of {excerpt.total_bytes}
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
    return <p className="session-turn-outcome">Turn cancelled</p>
  const excerpt =
    body.type === 'user_input' ? body.text : body.type === 'model_call' ? body.response : null
  if (!excerpt)
    return body.type === 'model_call' && body.provider_failure_cause ? (
      <p className="session-turn-outcome">
        Assistant error: {enumLabel(body.provider_failure_cause)}
      </p>
    ) : null
  return (
    <>
      <span className="eyebrow">{body.type === 'user_input' ? 'You' : 'Assistant'}</span>
      <p className="session-message-text">{excerpt.text}</p>
      {body.type === 'user_input' && <AttachmentReferences attachments={body.attachments} />}
      {(excerpt.offset_bytes !== '0' || excerpt.continuation !== null) && (
        <small>
          Showing from byte {excerpt.offset_bytes} of {excerpt.total_bytes}
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
          Couldn't load the transcript.{' '}
          <button type="button" onClick={() => void transcript.refetch()}>
            Retry transcript text
          </button>
        </p>
      )}
      {transcript.data?.items.map((item) => (
        <div
          key={conversationEntryKey(item)}
          className="session-message-entry"
          data-event-sequence={item.address.event_sequence}
        >
          <BodyText body={item.body} />
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
