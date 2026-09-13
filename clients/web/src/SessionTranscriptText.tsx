import { useInfiniteQuery, useQuery, useQueryClient } from '@tanstack/react-query'
import { type ReactNode, type RefObject, useEffect, useMemo, useRef, useState } from 'react'
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
  TRANSCRIPT_RETAINED_WINDOWS,
  TRANSCRIPT_WINDOW_BYTES,
  type TranscriptReadAnchor,
  TranscriptWindowReader,
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
  scrollRef?: RefObject<HTMLDivElement | null>
  sessionId: string
  first: string
  through: string
  observed: string
  limits: SessionTranscriptLimits
  eventSequence?: string
  anchor?: SessionWindowAnchor
  turnId?: string
  context?: CommandContext
  renderTool?: (tool: WebTimelineToolAttempt, detail: DetailMode) => ReactNode
}

export function SessionTranscriptText(props: SessionTranscriptTextProps) {
  return (
    <TranscriptWindow
      key={`${props.sessionId}:${props.eventSequence ?? ''}:${props.turnId ?? ''}:${JSON.stringify(props.anchor ?? null)}`}
      {...props}
    />
  )
}

function TranscriptWindow({
  scrollRef,
  sessionId,
  observed,
  limits,
  eventSequence,
  anchor,
}: SessionTranscriptTextProps) {
  const reader = useMemo(() => new TranscriptWindowReader(sessionId), [sessionId])
  const initialAnchor = useMemo<TranscriptReadAnchor>(
    () => (eventSequence ? { kind: 'around', eventSequence } : (anchor ?? { kind: 'latest' })),
    [eventSequence, anchor],
  )
  const queryKey = useMemo(
    () => ['production', 'scrolling-transcript', sessionId, initialAnchor, limits],
    [sessionId, initialAnchor, limits],
  )
  const queries = useQueryClient()
  const readerAtEnd = useRef(initialAnchor.kind === 'latest')
  const automaticLimits = useRef<SessionTranscriptLimits | undefined>(undefined)
  const transcript = useInfiniteQuery({
    queryKey,
    initialPageParam: initialAnchor,
    queryFn: ({ pageParam, signal }) => reader.read(pageParam, limits, signal),
    getPreviousPageParam: (page): TranscriptReadAnchor | undefined =>
      page.window.continuation_before
        ? {
            kind: 'before',
            detailLimits: automaticLimits.current,
            eventSequence: page.window.continuation_before.event_sequence,
          }
        : undefined,
    getNextPageParam: (page): SessionWindowAnchor | undefined =>
      page.window.continuation_after
        ? {
            kind: 'after',
            eventSequence: page.window.continuation_after.event_sequence,
          }
        : undefined,
    maxPages: TRANSCRIPT_RETAINED_WINDOWS,
    gcTime: 0,
  })
  useEffect(() => {
    if (transcript.error) console.error('Transcript load failed', transcript.error)
  }, [transcript.error])
  const pages = transcript.data?.pages
  const previousObservation = useRef(observed)
  useEffect(() => {
    if (previousObservation.current !== observed) {
      previousObservation.current = observed
      if (
        readerAtEnd.current &&
        (initialAnchor.kind === 'latest' || !pages?.at(-1)?.window.continuation_after)
      )
        queries.setQueryData<typeof transcript.data>(queryKey, (data) =>
          data
            ? {
                ...data,
                pageParams: [
                  { kind: 'latest' } satisfies SessionWindowAnchor,
                  ...data.pageParams.slice(1),
                ],
              }
            : data,
        )
      void transcript.refetch()
    }
  }, [observed, transcript.refetch, queries, queryKey, initialAnchor, pages])
  const entries = useMemo(
    () => pages?.flatMap((page) => page.details.flatMap((detail) => detail.items)) ?? [],
    [pages],
  )
  const visible = useMemo(
    () =>
      entries.filter(
        (item, index) =>
          hasConversationContent(item, entries.slice(0, index)) ||
          (item.body.type === 'tool_batch' &&
            pages?.some((page) =>
              page.details.some(
                (detail) => detail.items.at(-1) === item && detail.continuation !== null,
              ),
            )),
      ),
    [entries, pages],
  )
  const pending = useMemo(
    () =>
      pages?.flatMap((page) =>
        page.details.filter((detail) => detail.items.length === 0 && detail.continuation),
      ) ?? [],
    [pages],
  )
  const rows = useMemo(
    () =>
      [
        ...visible.map((item) => ({
          id: conversationEntryKey(item),
          sequence: item.address.event_sequence,
          item,
          pending: undefined,
        })),
        ...pending.map((page) => ({
          id: `pending-${continuationSequence(page)}`,
          sequence: continuationSequence(page),
          item: undefined,
          pending: page,
        })),
      ].sort((a, b) => {
        const left = BigInt(a.sequence)
        const right = BigInt(b.sequence)
        return left < right ? -1 : left > right ? 1 : 0
      }),
    [visible, pending],
  )
  const ids = useMemo(() => rows.map((row) => row.id), [rows])
  const selectedSequence =
    eventSequence ?? (initialAnchor.kind === 'around' ? initialAnchor.eventSequence : undefined)
  const selectedId = rows.find((row) => row.sequence === selectedSequence)?.id
  const emptyScanned = useRef({ headers: 0, items: 0, bytes: 0, first: '' })
  useEffect(() => {
    const oldest = pages?.[0]
    if (
      oldest?.details.some(
        (page) => page.continuation || page.items.some((item) => visible.includes(item)),
      )
    ) {
      emptyScanned.current = { headers: 0, items: 0, bytes: 0, first: '' }
      return
    }
    if (transcript.isFetching || transcript.isError || !transcript.hasPreviousPage) return
    const window = pages?.[0]?.window
    const first = window?.items[0]?.address.event_sequence
    if (first && first !== emptyScanned.current.first) {
      emptyScanned.current = {
        headers: emptyScanned.current.headers + (window?.items.length ?? 0),
        items:
          emptyScanned.current.items +
          (oldest?.details.reduce((sum, page) => sum + page.items.length, 0) ?? 0),
        bytes:
          emptyScanned.current.bytes +
          (oldest?.details.reduce((sum, page) => sum + page.projected_body_bytes, 0) ?? 0),
        first,
      }
    }
    const itemsLeft = Math.min(
      SESSION_WINDOW_ITEMS - emptyScanned.current.headers,
      Math.min(SESSION_WINDOW_ITEMS, limits.max_timeline_detail_items) - emptyScanned.current.items,
    )
    const bytesLeft =
      Math.min(TRANSCRIPT_WINDOW_BYTES, limits.max_timeline_detail_bytes) -
      emptyScanned.current.bytes
    if (itemsLeft < 1 || bytesLeft < limits.min_timeline_detail_bytes) return
    automaticLimits.current = {
      ...limits,
      max_timeline_detail_items: itemsLeft,
      max_timeline_detail_bytes: bytesLeft,
    }
    void transcript.fetchPreviousPage().finally(() => {
      automaticLimits.current = undefined
    })
  }, [
    visible,
    limits,
    pages,
    transcript.isFetching,
    transcript.isError,
    transcript.hasPreviousPage,
    transcript.fetchPreviousPage,
  ])
  return (
    <section
      className="session-transcript-text"
      aria-label="Transcript text"
      aria-busy={transcript.isFetching}
    >
      {transcript.isPending && <p role="status">Loading transcript…</p>}
      {transcript.isError && (
        <p role="alert">
          Transcript failed to load.{' '}
          <button type="button" onClick={() => void transcript.refetch()}>
            Retry transcript
          </button>
        </p>
      )}
      {!transcript.isPending && !transcript.isFetching && rows.length === 0 && (
        <p>No messages in this part of the conversation. Scroll up to keep looking.</p>
      )}
      <VirtualTranscript
        scrollRef={scrollRef}
        ids={ids}
        initialEnd={initialAnchor.kind === 'latest'}
        onEndChange={(atEnd) => {
          readerAtEnd.current = atEnd
        }}
        followEnd={
          (initialAnchor.kind === 'latest' || !pages?.at(-1)?.window.continuation_after) &&
          Boolean(
            pages
              ?.at(-1)
              ?.details.some(
                (page) => page.continuation || page.items.some((item) => visible.includes(item)),
              ),
          )
        }
        selectedId={selectedId}
        onEdge={(direction) => {
          if (transcript.isFetching || transcript.isError) return
          if (direction === 'before' && transcript.hasPreviousPage) {
            emptyScanned.current = { headers: 0, items: 0, bytes: 0, first: '' }
            void transcript.fetchPreviousPage()
          }
          if (direction === 'after' && transcript.hasNextPage) void transcript.fetchNextPage()
        }}
        renderRow={(index, measure, style) => {
          const row = rows[index]
          if (!row) return null
          if (row.pending)
            return (
              <div
                key={row.id}
                ref={measure}
                data-index={index}
                style={style}
                className="session-message-entry"
                data-event-sequence={row.sequence}
              >
                <ContinuedEvent sessionId={sessionId} page={row.pending} limits={limits} />
              </div>
            )
          const item = row.item
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
              {hasConversationContent(item, entries.slice(0, entries.indexOf(item))) && (
                <BodyText body={item.body} />
              )}
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

function continuationSequence(page: WebSessionTimelineDetailPage): string {
  const cursor = page.continuation
  return cursor?.type === 'more_at'
    ? cursor.address.event_sequence
    : (cursor?.body.address.event_sequence ?? '')
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
  const sequence = page.items.at(-1)?.address.event_sequence ?? continuationSequence(page)
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
