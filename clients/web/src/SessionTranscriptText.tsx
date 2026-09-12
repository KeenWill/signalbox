import { useInfiniteQuery, useQuery } from '@tanstack/react-query'
import { type ReactNode, useEffect, useMemo, useRef, useState } from 'react'
import { AttachmentReferences } from './AttachmentReferences'
import type { CommandContext } from './commands'
import type {
  WebSessionTimelineDetailBody,
  WebSessionTimelineDetailPage,
  WebTimelineTextExcerpt,
  WebTimelineToolAttempt,
} from './generated/web-contract.mjs'
import { enumLabel } from './labels'
import { readSessionTranscript, type SessionTranscriptLimits } from './product'
import { conversationEntryKey, hasConversationContent } from './session-timeline/conversation'
import type { SessionWindowAnchor } from './session-timeline/model'
import {
  readTranscriptWindow,
  TRANSCRIPT_RETAINED_WINDOWS,
  TRANSCRIPT_WINDOW_ITEMS,
} from './session-timeline/transcript'
import { SESSION_WINDOW_ITEMS } from './session-workspace'
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
  const visible = useMemo(
    () => entries.filter((item, index) => hasConversationContent(item, entries.slice(0, index))),
    [entries],
  )
  const ids = useMemo(() => visible.map(conversationEntryKey), [visible])
  const emptyScanned = useRef(0)
  useEffect(() => {
    if (visible.length > 0) {
      emptyScanned.current = 0
      return
    }
    if (
      transcript.isFetching ||
      transcript.isError ||
      !transcript.hasPreviousPage ||
      emptyScanned.current >= SESSION_WINDOW_ITEMS
    )
      return
    emptyScanned.current += TRANSCRIPT_WINDOW_ITEMS
    void transcript.fetchPreviousPage()
  }, [
    visible.length,
    transcript.isFetching,
    transcript.isError,
    transcript.hasPreviousPage,
    transcript.fetchPreviousPage,
  ])
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
      {!transcript.isPending && !transcript.isFetching && visible.length === 0 && (
        <p>No messages in this part of the conversation. Scroll up to keep looking.</p>
      )}
      <VirtualTranscript
        ids={ids}
        initialEnd={!eventSequence}
        selectedId={eventSequence}
        onEdge={(direction) => {
          if (transcript.isFetching || transcript.isError) return
          if (direction === 'before' && transcript.hasPreviousPage)
            void transcript.fetchPreviousPage()
          if (direction === 'after' && transcript.hasNextPage) void transcript.fetchNextPage()
        }}
        renderRow={(index, measure, style) => {
          const item = visible[index]
          if (!item) return null
          const detailPage = pages
            ?.flatMap((page) => page.details)
            .find((page) => page.items.at(-1) === item)
          return (
            <div
              key={conversationEntryKey(item)}
              ref={measure}
              data-index={index}
              style={style}
              className="session-message-entry"
              data-event-sequence={item.address.event_sequence}
            >
              <BodyText body={item.body} />
              {detailPage?.continuation && (
                <ContinuedEvent sessionId={sessionId} page={detailPage} limits={limits} />
              )}
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

function ContinuedEvent({
  sessionId,
  page,
  limits,
}: {
  sessionId: string
  page: WebSessionTimelineDetailPage
  limits: SessionTranscriptLimits
}) {
  const [open, setOpen] = useState(false)
  const [cursor, setCursor] = useState(page.continuation ?? null)
  const previous = useRef(page)
  const sequence = page.items.at(-1)?.address.event_sequence ?? ''
  const detail = useQuery({
    queryKey: ['production', 'transcript-continuation', sessionId, sequence, cursor, limits],
    enabled: open,
    queryFn: ({ signal }) =>
      readSessionTranscript(
        sessionId,
        sequence,
        sequence,
        cursor,
        limits,
        signal,
        previous.current,
      ),
    gcTime: 0,
  })
  if (!open)
    return (
      <button type="button" onClick={() => setOpen(true)}>
        Read more
      </button>
    )
  return (
    <section aria-label="More message text">
      {detail.isPending && <p role="status">Loading details…</p>}
      {detail.isError && (
        <p role="alert">
          Details could not be loaded.{' '}
          <button type="button" onClick={() => void detail.refetch()}>
            Retry details
          </button>
        </p>
      )}
      {detail.data?.items.map((item) => (
        <div key={conversationEntryKey(item)}>
          <BodyText body={item.body} />
        </div>
      ))}
      {detail.data?.continuation && (
        <button
          type="button"
          onClick={() => {
            if (detail.data) {
              previous.current = detail.data
              setCursor(detail.data.continuation ?? null)
            }
          }}
        >
          Continue reading
        </button>
      )}
      <button
        type="button"
        onClick={() => {
          setOpen(false)
          setCursor(page.continuation ?? null)
          previous.current = page
        }}
      >
        Close details
      </button>
    </section>
  )
}
