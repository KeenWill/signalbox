import { useQuery } from '@tanstack/react-query'
import { useState } from 'react'
import type {
  WebSessionTimelineDetailBody,
  WebTimelineDetailContinuation,
} from './generated/web-contract.mjs'
import { readSessionTranscript } from './product'

function BodyText({ body }: { body: WebSessionTimelineDetailBody }) {
  const excerpt =
    body.type === 'user_input' ? body.text : body.type === 'model_call' ? body.response : null
  if (!excerpt) return null
  return (
    <>
      <span className="eyebrow">{body.type === 'user_input' ? 'You' : 'Assistant'}</span>
      <p className="session-message-text">{excerpt.text}</p>
      {(excerpt.offset_bytes !== '0' || excerpt.continuation !== null) && (
        <small>
          Text excerpt · byte {excerpt.offset_bytes} of {excerpt.total_bytes}
        </small>
      )}
    </>
  )
}

export function SessionTranscriptText({
  sessionId,
  first,
  through,
  observed,
}: {
  sessionId: string
  first: string
  through: string
  observed: string
}) {
  const [continuation, setContinuation] = useState<WebTimelineDetailContinuation | null>(null)
  const transcript = useQuery({
    queryKey: ['production', 'session-text', sessionId, first, through, observed, continuation],
    queryFn: ({ signal }) => readSessionTranscript(sessionId, first, through, continuation, signal),
    gcTime: 0,
  })
  return (
    <section className="session-transcript-text" aria-label="Transcript text">
      {transcript.isPending && <p>Reading transcript text…</p>}
      {transcript.isError && (
        <p role="alert">
          Transcript text could not be read.{' '}
          <button type="button" onClick={() => void transcript.refetch()}>
            Retry transcript text
          </button>
        </p>
      )}
      {transcript.data?.items.map((item) => (
        <div key={item.address.event_sequence} className="session-message-entry">
          <BodyText body={item.body} />
        </div>
      ))}
      <div className="session-text-pagination">
        {continuation !== null && (
          <button type="button" onClick={() => setContinuation(null)}>
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
