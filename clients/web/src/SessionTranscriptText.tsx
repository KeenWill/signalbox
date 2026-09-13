import { useInfiniteQuery, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  type ReactNode,
  type RefObject,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react'
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
import { detailExcerptAt, type SessionWindowAnchor } from './session-timeline/model'
import {
  TRANSCRIPT_RETAINED_WINDOWS,
  TRANSCRIPT_WINDOW_BYTES,
  type TranscriptReadAnchor,
  TranscriptWindowReader,
} from './session-timeline/transcript'
import {
  groupTranscriptTurns,
  isToolBodyContinuation,
  isVisibleTurnEvent,
  type TranscriptTurn,
  toolContinuations,
  toolDisclosureKeys,
  toolEvidenceKey,
  turnSummaryParts,
} from './session-timeline/turns'

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

function advancesToolMember(page: WebSessionTimelineDetailPage): boolean {
  const item = page.items.at(-1)
  const cursor = page.continuation
  return (
    item?.body.type === 'tool_batch' &&
    cursor?.type === 'more_body' &&
    cursor.body.address.event_sequence === item.address.event_sequence &&
    cursor.body.member_index > (item.body.projected_member_index ?? 0)
  )
}

type LoadedToolPage = { page: WebSessionTimelineDetailPage; includeTools: boolean }
type AdoptToolPage = (
  source: WebSessionTimelineDetailPage,
  page: WebSessionTimelineDetailPage,
  includeTools: boolean,
) => void

function TranscriptWindow({
  scrollRef,
  sessionId,
  observed,
  limits,
  eventSequence,
  renderTool,
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
  const followLatest = useRef(false)
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
    getNextPageParam: (page): TranscriptReadAnchor | undefined =>
      page.window.continuation_after
        ? {
            kind: 'after',
            detailLimits: automaticLimits.current,
            eventSequence: page.window.continuation_after.event_sequence,
          }
        : undefined,
    maxPages: TRANSCRIPT_RETAINED_WINDOWS,
    gcTime: 0,
  })
  const pageRead = useRef(false)
  const scanDirection = useRef<'before' | 'after'>(
    initialAnchor.kind === 'first' || initialAnchor.kind === 'after' ? 'after' : 'before',
  )
  const readPage = useCallback(
    (direction: 'before' | 'after') => {
      if (pageRead.current) return
      pageRead.current = true
      const fetchPage =
        direction === 'before' ? transcript.fetchPreviousPage : transcript.fetchNextPage
      void fetchPage().finally(() => {
        pageRead.current = false
        automaticLimits.current = undefined
      })
    },
    [transcript.fetchPreviousPage, transcript.fetchNextPage],
  )
  useEffect(() => {
    if (transcript.error) console.error('Transcript load failed', transcript.error)
  }, [transcript.error])
  const pages = transcript.data?.pages
  const previousObservation = useRef(observed)
  useEffect(() => {
    if (previousObservation.current !== observed) {
      previousObservation.current = observed
      if (readerAtEnd.current && followLatest.current)
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
  }, [observed, transcript.refetch, queries, queryKey])
  const [toolPages, setToolPages] = useState<Record<string, LoadedToolPage>>({})
  const adoptToolPage = useCallback<AdoptToolPage>((source, page, includeTools) => {
    const key = JSON.stringify(source.continuation)
    setToolPages((current) =>
      current[key]?.page === page
        ? current
        : {
            ...current,
            [key]: { page, includeTools },
          },
    )
  }, [])
  const retainedEntries = useMemo(
    () => pages?.flatMap((page) => page.details.flatMap((detail) => detail.items)) ?? [],
    [pages],
  )
  useEffect(() => {
    const retained = new Set(retainedEntries.map((event) => event.address.event_sequence))
    setToolPages((current) =>
      Object.values(current).some(({ page }) =>
        page.items.some((item) => !retained.has(item.address.event_sequence)),
      )
        ? Object.fromEntries(
            Object.entries(current).filter(([, { page }]) =>
              page.items.every((item) => retained.has(item.address.event_sequence)),
            ),
          )
        : current,
    )
  }, [retainedEntries])
  const entries = useMemo(
    () =>
      retainedEntries.flatMap((event) => [
        event,
        ...Object.values(toolPages).flatMap(({ page, includeTools }) =>
          includeTools
            ? page.items.filter(
                (item) => item.address.event_sequence === event.address.event_sequence,
              )
            : [],
        ),
      ]),
    [retainedEntries, toolPages],
  )
  const detailPages = useMemo(
    () =>
      [
        ...(pages?.flatMap((page) => page.details) ?? []),
        ...Object.values(toolPages).map(({ page }) => page),
      ].filter((page) => !toolPages[JSON.stringify(page.continuation)]?.includeTools),
    [pages, toolPages],
  )
  const windowStarts = useMemo(
    () =>
      new Set(
        pages?.flatMap((page) => {
          const first = page.details.flatMap((detail) => detail.items)[0]
          return first ? [first.address.event_sequence] : []
        }) ?? [],
      ),
    [pages],
  )
  const toolSegments = useRef(new Map<string, string>())
  const turns = useMemo(
    () =>
      groupTranscriptTurns(entries, windowStarts, toolSegments.current).filter(
        (turn) =>
          turn.messages.length > 0 ||
          turn.result ||
          turn.tools.length > 0 ||
          turn.warnings.length > 0 ||
          turn.outcome,
      ),
    [entries, windowStarts],
  )
  useEffect(() => {
    toolSegments.current = new Map(
      turns.flatMap((turn) => turn.tools.map((tool) => [toolEvidenceKey(tool), turn.id] as const)),
    )
  }, [turns])
  const pending = useMemo(
    () =>
      pages?.flatMap((page) =>
        page.details.filter((detail) => {
          const last = detail.items.at(-1)
          return (
            detail.continuation &&
            (!last ||
              (last.body.type !== 'tool_batch' &&
                !turns.some((turn) => isVisibleTurnEvent(turn, last))))
          )
        }),
      ) ?? [],
    [pages, turns],
  )
  const rows = useMemo(
    () =>
      [
        ...turns.flatMap((turn) => {
          const first = turn.events[0]
          return first
            ? [{ id: turn.id, sequence: first.address.event_sequence, turn, pending: undefined }]
            : []
        }),
        ...pending.map((page) => ({
          id: `pending-${continuationSequence(page)}`,
          sequence: continuationSequence(page),
          turn: undefined,
          pending: page,
        })),
      ].sort((a, b) => {
        const left = BigInt(a.sequence)
        const right = BigInt(b.sequence)
        return left < right ? -1 : left > right ? 1 : 0
      }),
    [turns, pending],
  )
  const ids = useMemo(() => rows.map((row) => row.id), [rows])
  useEffect(() => {
    if (readerAtEnd.current && pages && !pages.at(-1)?.window.continuation_after)
      followLatest.current = true
  }, [pages])
  const selectedSequence =
    eventSequence ?? (initialAnchor.kind === 'around' ? initialAnchor.eventSequence : undefined)
  const selectedId =
    turns.find((turn) =>
      turn.events.some((event) => event.address.event_sequence === selectedSequence),
    )?.id ?? rows.find((row) => row.sequence === selectedSequence)?.id
  const emptyScanned = useRef({ headers: 0, items: 0, bytes: 0, first: '' })
  useEffect(() => {
    const direction = scanDirection.current
    const boundary = direction === 'before' ? pages?.[0] : pages?.at(-1)
    if (
      boundary?.details.some(
        (page) =>
          page.continuation ||
          page.items.some((item) => turns.some((turn) => isVisibleTurnEvent(turn, item))),
      )
    ) {
      emptyScanned.current = { headers: 0, items: 0, bytes: 0, first: '' }
      return
    }
    if (
      pageRead.current ||
      transcript.isFetching ||
      transcript.isError ||
      !(direction === 'before' ? transcript.hasPreviousPage : transcript.hasNextPage)
    )
      return
    const window = boundary?.window
    const first = window?.items[0]?.address.event_sequence
    if (first && first !== emptyScanned.current.first) {
      emptyScanned.current = {
        headers: emptyScanned.current.headers + (window?.items.length ?? 0),
        items:
          emptyScanned.current.items +
          (boundary?.details.reduce((sum, page) => sum + page.items.length, 0) ?? 0),
        bytes:
          emptyScanned.current.bytes +
          (boundary?.details.reduce((sum, page) => sum + page.projected_body_bytes, 0) ?? 0),
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
    readPage(direction)
  }, [
    turns,
    limits,
    pages,
    transcript.isFetching,
    transcript.isError,
    transcript.hasPreviousPage,
    transcript.hasNextPage,
    readPage,
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
        <p>No messages in this part of the conversation. Keep scrolling to look for messages.</p>
      )}
      <VirtualTranscript
        scrollRef={scrollRef}
        ids={ids}
        loadingLater={transcript.isFetchingNextPage}
        initialEnd={initialAnchor.kind === 'latest'}
        onEndChange={(atEnd) => {
          readerAtEnd.current = atEnd
          if (atEnd && pages && !pages.at(-1)?.window.continuation_after)
            followLatest.current = true
        }}
        followEnd={
          !pages?.at(-1)?.window.continuation_after &&
          Boolean(
            pages
              ?.at(-1)
              ?.details.some(
                (page) =>
                  page.continuation ||
                  page.items.some((item) => turns.some((turn) => isVisibleTurnEvent(turn, item))),
              ),
          )
        }
        selectedId={selectedId}
        onEdge={(direction) => {
          if (pageRead.current || transcript.isFetching || transcript.isError) return
          if (!(direction === 'before' ? transcript.hasPreviousPage : transcript.hasNextPage))
            return
          if (direction === 'before') followLatest.current = false
          scanDirection.current = direction
          emptyScanned.current = { headers: 0, items: 0, bytes: 0, first: '' }
          readPage(direction)
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
          const turn = row.turn
          return (
            <div
              key={turn.id}
              ref={measure}
              data-index={index}
              style={style}
              className="session-message-entry session-turn"
              data-turn-id={turn.turnId}
            >
              <TurnSummary
                turn={turn}
                renderTool={renderTool}
                detailPages={detailPages}
                adoptToolPage={adoptToolPage}
                sessionId={sessionId}
                limits={limits}
              />
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
  adoptToolPage,
  turn,
  renderTool,
  detailPages,
  sessionId,
  limits,
}: {
  adoptToolPage: AdoptToolPage
  turn: TranscriptTurn
  renderTool?: SessionTranscriptTextProps['renderTool']
  detailPages: readonly WebSessionTimelineDetailPage[]
  sessionId: string
  limits: SessionTranscriptLimits
}) {
  const [openTool, setOpenTool] = useState<string | null>(null)
  const previousKeys = useRef<ReadonlyMap<string, string>>(new Map())
  const disclosureKeys = useMemo(
    () => toolDisclosureKeys(turn.tools, previousKeys.current),
    [turn.tools],
  )
  useEffect(() => {
    previousKeys.current = disclosureKeys
  }, [disclosureKeys])
  const disclosureKey = (entry: WebTimelineToolAttempt) =>
    disclosureKeys.get(toolEvidenceKey(entry)) ?? toolEvidenceKey(entry)
  const tool = turn.tools.find((entry) => disclosureKey(entry) === openTool)
  const more = (sequence: string) => {
    const page = detailPages.find((page) => page.items.at(-1)?.address.event_sequence === sequence)
    return page?.continuation ? (
      <ContinuedEvent sessionId={sessionId} page={page} limits={limits} />
    ) : null
  }
  return (
    <>
      {turnSummaryParts(turn).map((part) =>
        part.kind === 'message' ? (
          <div
            key={conversationEntryKey(part.item)}
            data-event-sequence={part.item.address.event_sequence}
          >
            <BodyText body={part.item.body} />
            {more(part.item.address.event_sequence)}
          </div>
        ) : (
          <section
            className="session-tool-chips"
            aria-label="Tools used"
            key={part.tools[0] ? disclosureKey(part.tools[0]) : undefined}
          >
            {part.tools.map((entry) => (
              <button
                type="button"
                key={disclosureKey(entry)}
                aria-expanded={openTool === disclosureKey(entry)}
                onClick={() =>
                  setOpenTool(openTool === disclosureKey(entry) ? null : disclosureKey(entry))
                }
              >
                {entry.tool_name}
              </button>
            ))}
            {tool &&
              part.tools.some((entry) => toolEvidenceKey(entry) === toolEvidenceKey(tool)) && (
                <div className="session-tool-slot">
                  {renderTool ? renderTool(tool, 'condensed') : <ToolSummary tool={tool} />}
                  {detailPages
                    .filter((candidate) => {
                      const cursor = candidate.continuation
                      const item = candidate.items.at(-1)
                      return (
                        cursor?.type === 'more_body' &&
                        isToolBodyContinuation(cursor.body) &&
                        !advancesToolMember(candidate) &&
                        ((item?.body.type === 'tool_batch' &&
                          item.body.tools.some(
                            (entry) => disclosureKey(entry) === disclosureKey(tool),
                          )) ||
                          toolContinuations(tool).some(
                            ({ continuation }) =>
                              cursor.body.address.event_sequence ===
                                continuation.address.event_sequence &&
                              cursor.body.field === continuation.field &&
                              cursor.body.member_index === continuation.member_index &&
                              cursor.body.offset_bytes === continuation.offset_bytes,
                          ))
                      )
                    })
                    .map((page) => (
                      <ContinuedEvent
                        key={JSON.stringify(page.continuation)}
                        adoptToolPage={adoptToolPage}
                        sessionId={sessionId}
                        page={page}
                        limits={limits}
                      />
                    ))}
                </div>
              )}
            {detailPages
              .filter(
                (page) =>
                  advancesToolMember(page) &&
                  turn.events.some(
                    (event) =>
                      event.address.event_sequence === continuationSequence(page) &&
                      event.body.type === 'tool_batch' &&
                      event.body.tools.some((entry) =>
                        part.tools.some((tool) => disclosureKey(tool) === disclosureKey(entry)),
                      ),
                  ),
              )
              .map((page) => (
                <MoreTools
                  key={JSON.stringify(page.continuation)}
                  sessionId={sessionId}
                  page={page}
                  limits={limits}
                  adoptToolPage={adoptToolPage}
                />
              ))}
          </section>
        ),
      )}
      {turn.outcome && <BodyText body={turn.outcome.body} />}
    </>
  )
}

function ToolSummary({ tool }: { tool: WebTimelineToolAttempt }) {
  const evidence = tool.evidence.type === 'physical_attempt' ? tool.evidence : null
  return (
    <section aria-label={`${tool.tool_name} details`}>
      <strong>{tool.tool_name}</strong>
      <small>Tool summaries</small>
      {tool.arguments && <ToolText label="Arguments" excerpt={tool.arguments} />}
      {evidence?.result && <ToolText label="Output" excerpt={evidence.result} />}
      {evidence?.failure && <ToolText label="Failure" excerpt={evidence.failure} />}
      {evidence && !evidence.result && !evidence.failure && (
        <p className="session-turn-outcome">
          {evidence.state === 'known_failed' || evidence.failure_present
            ? `Failure · ${enumLabel(evidence.cause ?? evidence.state)}`
            : enumLabel(evidence.state)}
        </p>
      )}
      {[tool.arguments, evidence?.result, evidence?.failure].some(
        (excerpt) => excerpt && (excerpt.offset_bytes !== '0' || excerpt.continuation != null),
      ) && <small>Excerpt · more text available</small>}
    </section>
  )
}

function continuationSequence(page: WebSessionTimelineDetailPage): string {
  const cursor = page.continuation
  return cursor?.type === 'more_at'
    ? cursor.address.event_sequence
    : (cursor?.body.address.event_sequence ?? '')
}

function MoreTools({
  sessionId,
  page,
  limits,
  adoptToolPage,
}: {
  sessionId: string
  page: WebSessionTimelineDetailPage
  limits: SessionTranscriptLimits
  adoptToolPage: AdoptToolPage
}) {
  const [open, setOpen] = useState(false)
  const sequence = continuationSequence(page)
  const detail = useQuery({
    queryKey: ['production', 'transcript-batch-member', sessionId, page.continuation, limits],
    enabled: open,
    queryFn: ({ signal }) =>
      readSessionTranscript(
        sessionId,
        sequence,
        sequence,
        page.continuation ?? null,
        limits,
        signal,
        page,
      ),
    gcTime: 0,
  })
  useEffect(() => {
    if (detail.data) adoptToolPage(page, detail.data, true)
  }, [detail.data, page, adoptToolPage])
  return (
    <>
      {detail.isError && <p role="alert">More tools could not be loaded.</p>}
      <button
        type="button"
        disabled={detail.isFetching}
        onClick={() => {
          if (open) void detail.refetch()
          else setOpen(true)
        }}
      >
        {detail.isFetching
          ? 'Loading tools…'
          : detail.isError
            ? 'Retry more tools'
            : 'Show more tools'}
      </button>
    </>
  )
}

function ContinuedEvent({
  adoptToolPage,
  sessionId,
  page,
  limits,
}: {
  adoptToolPage?: AdoptToolPage
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
  useEffect(() => {
    if (adoptToolPage && detail.data && advancesToolMember(detail.data))
      adoptToolPage(previous.current, detail.data, false)
  }, [detail.data, adoptToolPage])
  const next = detail.data?.continuation
  const canContinue =
    detail.data &&
    next &&
    (!adoptToolPage ||
      (next.type === 'more_body' &&
        isToolBodyContinuation(next.body) &&
        !advancesToolMember(detail.data)))
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
      {detail.data?.items.map((item) => {
        const excerpt =
          cursor?.type === 'more_body' && !hasConversationContent(item)
            ? detailExcerptAt(item.body, cursor.body)
            : null
        return (
          <div key={conversationEntryKey(item)}>
            {excerpt ? (
              <ToolText label="Details" excerpt={excerpt} />
            ) : (
              <BodyText body={item.body} />
            )}
          </div>
        )
      })}
      {canContinue && (
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
