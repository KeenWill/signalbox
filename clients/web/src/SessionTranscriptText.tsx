import { useInfiniteQuery, useQuery, useQueryClient } from '@tanstack/react-query'
import { useLocation } from '@tanstack/react-router'
import { type ReactNode, type RefObject, useEffect, useMemo, useRef, useState } from 'react'
import { AttachmentReferences } from './AttachmentReferences'
import { type CommandContext, invokeCommand } from './commands'
import type {
  WebSessionTimelineDetail,
  WebSessionTimelineDetailBody,
  WebSessionTimelineDetailPage,
  WebTimelineDetailContinuation,
  WebTimelineTextExcerpt,
  WebTimelineToolAttempt,
} from './generated/web-contract.mjs'
import { enumLabel } from './labels'
import { readSessionTranscript, type SessionTranscriptLimits } from './product'
import { GoalEventDetail } from './SessionItemDetail'
import { conversationEntryKey } from './session-timeline/conversation'
import type { SessionWindowAnchor } from './session-timeline/model'
import {
  TRANSCRIPT_RETAINED_WINDOWS,
  TRANSCRIPT_WINDOW_BYTES,
  type TranscriptReadAnchor,
  TranscriptWindowReader,
} from './session-timeline/transcript'
import { readTurnTranscript } from './session-timeline/turn-detail'
import {
  groupTranscriptTurns,
  type TranscriptTurn,
  toolContinuations,
  toolEvidenceKey,
  turnSummaryParts,
} from './session-timeline/turns'

import { SESSION_WINDOW_ITEMS } from './session-workspace'
import { type DetailMode, store, useAppDispatch, useAppSelector } from './state'
import { VirtualTranscript } from './Transcript'

type EventContinuationState = {
  cursor: WebTimelineDetailContinuation | null
  earlier: { key: string; item: WebSessionTimelineDetail }[]
  previous?: WebSessionTimelineDetailPage
  current?: { key: string; page: WebSessionTimelineDetailPage }
}

type ContinuedEventState = {
  sequence: string
  cursor: WebTimelineDetailContinuation | null
  earlier: { key: string; items: WebSessionTimelineDetailPage['items'] }[]
  previous: WebSessionTimelineDetailPage
  current?: WebSessionTimelineDetailPage
}

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
        {body.goal_events.length > 0 && (
          <section aria-label="Goal events">
            {body.goal_events.map((event) => (
              <GoalEventDetail key={`${event.generation}:${event.type}`} event={event} />
            ))}
          </section>
        )}
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
  registerUnwind?: (unwind: () => boolean) => () => void
  renderTool?: (tool: WebTimelineToolAttempt, detail: DetailMode) => ReactNode
}

export function SessionTranscriptText(props: SessionTranscriptTextProps) {
  const search = useLocation({ select: (location) => location.searchStr })
  const params = new URLSearchParams(search)
  const address = props.eventSequence ?? params.get('around') ?? undefined
  const eventSequence = address && /^[1-9]\d{0,19}$/.test(address) ? address : undefined
  const requestedTurn = props.turnId ?? params.get('turn') ?? undefined
  const turnId =
    requestedTurn &&
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(requestedTurn)
      ? requestedTurn.toLowerCase()
      : undefined
  const location = useQuery({
    queryKey: ['production', 'transcript-turn-location', props.sessionId, turnId, props.limits],
    enabled: Boolean(turnId && !eventSequence),
    queryFn: ({ signal }) =>
      readTurnTranscript(props.sessionId, turnId ?? '', null, props.limits, signal),
    gcTime: 0,
  })
  if (turnId && !eventSequence && location.isPending) return <p role="status">Finding turn…</p>
  if (turnId && !eventSequence && location.isError)
    return (
      <p role="alert">
        Turn could not be loaded.{' '}
        <button type="button" onClick={() => void location.refetch()}>
          Retry turn
        </button>
      </p>
    )
  const target = eventSequence ?? location.data?.items[0]?.address.event_sequence
  return (
    <TranscriptWindow
      key={`${props.sessionId}:${target ?? ''}:${turnId ?? ''}:${JSON.stringify(props.anchor ?? null)}`}
      {...props}
      eventSequence={target}
      turnId={turnId}
    />
  )
}

function TranscriptWindow({
  scrollRef,
  sessionId,
  observed,
  limits,
  eventSequence,
  renderTool,
  turnId: requestedTurn,
  context,
  registerUnwind,
  anchor,
}: SessionTranscriptTextProps) {
  const detail = useAppSelector((state) => state.app.detail)
  const dispatch = useAppDispatch()
  const [turnModes, setTurnModes] = useState<Record<string, DetailMode>>({})
  const [eventContinuations, setEventContinuations] = useState<
    Record<string, EventContinuationState>
  >({})
  const [continuedEvents, setContinuedEvents] = useState<Record<string, ContinuedEventState>>({})
  const [openTools, setOpenTools] = useState<Record<string, string | null>>({})
  const surface = useRef<HTMLElement>(null)
  const commandContext: CommandContext = {
    dispatch,
    getState: store.getState,
    timelineIds: [],
    artifactPreviewIds: [],
    artifactOriginalIds: [],
    focusTimeline: () => {},
    ...context,
    configuresTranscriptDetail: true,
  }

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
        ? { kind: 'after', eventSequence: page.window.continuation_after.event_sequence }
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
  const turns = useMemo(
    () =>
      groupTranscriptTurns(entries).filter(
        (turn) =>
          detail === 'full' ||
          turnModes[turn.turnId ?? turn.id] === 'full' ||
          turn.events.some((event) => event.address.event_sequence === eventSequence) ||
          turn.messages.length > 0 ||
          turn.result ||
          turn.tools.length > 0 ||
          turn.warnings.length > 0 ||
          turn.outcome,
      ),
    [entries, detail, turnModes, eventSequence],
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
  const renderContinuation = (page: WebSessionTimelineDetailPage) => {
    const key = JSON.stringify([continuationSequence(page), page.continuation])
    return (
      <ContinuedEvent
        key={key}
        sessionId={sessionId}
        page={page}
        limits={limits}
        state={continuedEvents[key]}
        onChange={(state) =>
          setContinuedEvents((current) => {
            if (state) return { ...current, [key]: state }
            const next = { ...current }
            delete next[key]
            return next
          })
        }
      />
    )
  }

  useEffect(() => {
    const loaded = new Set(
      entries.map((entry) => {
        const turn = groupTranscriptTurns([entry])[0]
        return turn?.turnId ?? turn?.id
      }),
    )
    setTurnModes((current) =>
      Object.keys(current).some((id) => !loaded.has(id))
        ? Object.fromEntries(Object.entries(current).filter(([id]) => loaded.has(id)))
        : current,
    )
    const loadedEvents = new Set(entries.map((entry) => entry.address.event_sequence))
    const continuedSequences = new Set([...loadedEvents, ...pending.map(continuationSequence)])
    setContinuedEvents((current) =>
      Object.values(current).some((state) => !continuedSequences.has(state.sequence))
        ? Object.fromEntries(
            Object.entries(current).filter(([, state]) => continuedSequences.has(state.sequence)),
          )
        : current,
    )
    const loadedSegments = new Set(groupTranscriptTurns(entries).map((turn) => turn.id))
    setOpenTools((current) =>
      Object.keys(current).some((id) => !loadedSegments.has(id))
        ? Object.fromEntries(Object.entries(current).filter(([id]) => loadedSegments.has(id)))
        : current,
    )

    setEventContinuations((current) =>
      Object.keys(current).some((sequence) => !loadedEvents.has(sequence))
        ? Object.fromEntries(
            Object.entries(current).filter(([sequence]) => loadedEvents.has(sequence)),
          )
        : current,
    )
  }, [entries, pending])
  const previousDetail = useRef(detail)
  useEffect(() => {
    if (previousDetail.current === detail) return
    previousDetail.current = detail
    setTurnModes({})
    setEventContinuations({})
    setContinuedEvents({})
    setOpenTools({})
  }, [detail])
  useEffect(
    () =>
      registerUnwind?.(() => {
        const row = document.activeElement?.closest<HTMLElement>('[data-transcript-turn]')
        if (!row || !surface.current?.contains(row)) return false
        const id = row.dataset.transcriptTurn
        const turn = turns.find((turn) => turn.id === id)
        if (
          !turn ||
          (detail !== 'full' &&
            turnModes[turn.turnId ?? turn.id] !== 'full' &&
            requestedTurn !== turn.turnId &&
            !turn.events.some((event) => event.address.event_sequence === eventSequence))
        )
          return false
        if (
          turnModes[turn.turnId ?? turn.id] === 'results' ||
          turnModes[turn.turnId ?? turn.id] === 'condensed'
        )
          return false
        setTurnModes((current) => ({
          ...current,
          [turn.turnId ?? turn.id]: detail === 'full' ? 'results' : detail,
        }))
        const sequences = new Set(turn.events.map((event) => event.address.event_sequence))
        setEventContinuations((current) =>
          Object.keys(current).some((sequence) => sequences.has(sequence))
            ? Object.fromEntries(
                Object.entries(current).filter(([sequence]) => !sequences.has(sequence)),
              )
            : current,
        )
        requestAnimationFrame(() =>
          row.querySelector<HTMLButtonElement>('.session-turn-heading button')?.focus(),
        )
        return true
      }),
    [registerUnwind, turns, turnModes, requestedTurn, eventSequence, detail],
  )
  const selectedSequence =
    eventSequence ?? (initialAnchor.kind === 'around' ? initialAnchor.eventSequence : undefined)
  const selectedId =
    turns.find((turn) =>
      turn.events.some((event) => event.address.event_sequence === selectedSequence),
    )?.id ?? rows.find((row) => row.sequence === selectedSequence)?.id
  const emptyScanned = useRef({ headers: 0, items: 0, bytes: 0, first: '' })
  useEffect(() => {
    const oldest = pages?.[0]
    if (
      oldest?.details.some(
        (page) =>
          page.continuation ||
          page.items.some((item) =>
            turns.some(
              (turn) =>
                (turn.events.includes(item) &&
                  (detail === 'full' ||
                    turnModes[turn.turnId ?? turn.id] === 'full' ||
                    item.address.event_sequence === eventSequence)) ||
                turn.messages.includes(item) ||
                turn.result === item ||
                turn.warnings.includes(item) ||
                turn.outcome === item ||
                (item.body.type === 'tool_batch' && turn.events.includes(item)),
            ),
          ),
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
    turns,
    detail,
    turnModes,
    eventSequence,
    limits,
    pages,
    transcript.isFetching,
    transcript.isError,
    transcript.hasPreviousPage,
    transcript.fetchPreviousPage,
  ])
  return (
    <section
      ref={surface}
      className="session-transcript-text"
      aria-label="Transcript text"
      aria-busy={transcript.isFetching}
    >
      <fieldset className="session-transcript-levels">
        <legend>Show</legend>
        {(
          [
            ['results', 'Summary'],
            ['condensed', 'Tools'],
            ['full', 'All details'],
          ] as const
        ).map(([mode, label]) => (
          <label key={mode}>
            <input
              type="radio"
              name={`transcript-level-${sessionId}`}
              checked={detail === mode}
              onChange={() => invokeCommand(`detail.${mode}`, commandContext)}
            />
            {label}
          </label>
        ))}
      </fieldset>
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
          !pages?.at(-1)?.window.continuation_after &&
          Boolean(
            pages
              ?.at(-1)
              ?.details.some(
                (page) =>
                  page.continuation ||
                  page.items.some((item) =>
                    turns.some(
                      (turn) =>
                        (turn.events.includes(item) &&
                          (detail === 'full' || turnModes[turn.turnId ?? turn.id] === 'full')) ||
                        turn.messages.includes(item) ||
                        turn.result === item ||
                        turn.warnings.includes(item) ||
                        turn.outcome === item ||
                        (item.body.type === 'tool_batch' && turn.events.includes(item)),
                    ),
                  ),
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
                {renderContinuation(row.pending)}
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
              data-transcript-turn={turn.id}
            >
              <TurnContent
                turn={turn}
                collapsedDetail={detail === 'full' ? 'results' : detail}
                detail={
                  turnModes[turn.turnId ?? turn.id] ??
                  (detail === 'full' ||
                  requestedTurn === turn.turnId ||
                  turn.events.some((event) => event.address.event_sequence === eventSequence)
                    ? 'full'
                    : detail)
                }
                detailPages={pages?.flatMap((page) => page.details) ?? []}
                renderTool={renderTool}
                sessionId={sessionId}
                limits={limits}
                target={eventSequence}
                renderContinuation={renderContinuation}
                openTool={openTools[turn.id] ?? null}
                onOpenTool={(tool) => {
                  const closed = openTools[turn.id]
                  setOpenTools((current) => ({ ...current, [turn.id]: tool }))
                  if (closed)
                    setContinuedEvents((current) =>
                      Object.fromEntries(
                        Object.entries(current).filter(
                          ([, state]) =>
                            !state.previous.items.some(
                              (item) =>
                                item.body.type === 'tool_batch' &&
                                item.body.tools.some((tool) => toolEvidenceKey(tool) === closed),
                            ),
                        ),
                      ),
                    )
                }}
                eventContinuations={eventContinuations}
                onEventContinuation={(sequence, state) =>
                  setEventContinuations((current) => ({ ...current, [sequence]: state }))
                }
                onExpand={(mode) => {
                  const row =
                    surface.current?.querySelectorAll<HTMLElement>('[data-transcript-turn]')
                  const target = Array.from(row ?? []).find(
                    (row) => row.dataset.transcriptTurn === turn.id,
                  )
                  setTurnModes((current) => ({ ...current, [turn.turnId ?? turn.id]: mode }))
                  setOpenTools((current) => ({ ...current, [turn.id]: null }))
                  const closedEvents = new Set(
                    turn.events.map((event) => event.address.event_sequence),
                  )
                  setContinuedEvents((current) =>
                    Object.fromEntries(
                      Object.entries(current).filter(
                        ([, state]) => !closedEvents.has(state.sequence),
                      ),
                    ),
                  )
                  if (mode !== 'full') {
                    const sequences = new Set(
                      turn.events.map((event) => event.address.event_sequence),
                    )
                    setEventContinuations((current) =>
                      Object.keys(current).some((sequence) => sequences.has(sequence))
                        ? Object.fromEntries(
                            Object.entries(current).filter(
                              ([sequence]) => !sequences.has(sequence),
                            ),
                          )
                        : current,
                    )
                  }
                  requestAnimationFrame(() =>
                    target
                      ?.querySelector<HTMLButtonElement>('.session-turn-heading button')
                      ?.focus(),
                  )
                }}
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

function TurnContent({
  turn,
  detailPages,
  collapsedDetail,
  renderTool,
  detail,
  sessionId,
  limits,
  target,
  renderContinuation,
  openTool,
  onOpenTool,
  eventContinuations,
  onEventContinuation,
  onExpand,
}: {
  turn: TranscriptTurn
  collapsedDetail: DetailMode
  detailPages: readonly WebSessionTimelineDetailPage[]
  renderTool?: SessionTranscriptTextProps['renderTool']
  detail: DetailMode
  sessionId: string
  limits: SessionTranscriptLimits
  target?: string
  renderContinuation: (page: WebSessionTimelineDetailPage) => ReactNode
  openTool: string | null
  onOpenTool: (tool: string | null) => void
  eventContinuations: Readonly<Record<string, EventContinuationState>>
  onEventContinuation: (sequence: string, state: EventContinuationState) => void
  onExpand: (mode: DetailMode) => void
}) {
  const tool = turn.tools.find((entry) => toolEvidenceKey(entry) === openTool)
  const more = (sequence: string) => {
    const page = detailPages.find((page) => page.items.at(-1)?.address.event_sequence === sequence)
    return page?.continuation ? renderContinuation(page) : null
  }
  const moreTool = (tool: WebTimelineToolAttempt) =>
    detailPages
      .filter((candidate) => {
        const cursor = candidate.continuation
        const item = candidate.items.at(-1)
        return (
          cursor?.type === 'more_body' &&
          ((item?.body.type === 'tool_batch' &&
            item.body.tools.some((entry) => toolEvidenceKey(entry) === toolEvidenceKey(tool))) ||
            toolContinuations(tool).some(
              ({ continuation }) =>
                cursor.body.address.event_sequence === continuation.address.event_sequence &&
                cursor.body.field === continuation.field &&
                cursor.body.member_index === continuation.member_index &&
                cursor.body.offset_bytes === continuation.offset_bytes,
            ))
        )
      })
      .map(renderContinuation)
  const events = [
    ...new Map(turn.events.map((event) => [event.address.event_sequence, event])).values(),
  ]
  if (detail === 'full')
    return (
      <>
        <header className="session-turn-heading">
          <a
            href={`/sessions?workspace=true&session=${sessionId}&around=${events[0]?.address.event_sequence ?? ''}${turn.turnId ? `&turn=${turn.turnId}` : ''}`}
          >
            Link to turn
          </a>
          <button type="button" onClick={() => onExpand(collapsedDetail)}>
            Collapse turn
          </button>
        </header>
        {events.map((event) => (
          <EventDetail
            key={event.address.event_sequence}
            event={event}
            sessionId={sessionId}
            turnId={turn.turnId}
            limits={limits}
            renderTool={renderTool}
            target={target === event.address.event_sequence}
            continuation={eventContinuations[event.address.event_sequence]}
            onContinuation={(state) => onEventContinuation(event.address.event_sequence, state)}
          />
        ))}
      </>
    )
  return (
    <>
      <header className="session-turn-heading">
        <button type="button" onClick={() => onExpand('full')}>
          Open turn details
        </button>
      </header>
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
            key={part.tools[0] ? toolEvidenceKey(part.tools[0]) : undefined}
          >
            {part.tools.map((entry) => (
              <button
                type="button"
                key={toolEvidenceKey(entry)}
                aria-expanded={
                  detail === 'condensed' ? undefined : openTool === toolEvidenceKey(entry)
                }
                aria-label={
                  detail === 'condensed' ? `Open turn details for ${entry.tool_name}` : undefined
                }
                onClick={() =>
                  detail === 'condensed'
                    ? onExpand('full')
                    : onOpenTool(
                        openTool === toolEvidenceKey(entry) ? null : toolEvidenceKey(entry),
                      )
                }
              >
                {entry.tool_name}
              </button>
            ))}
            {detail === 'condensed' &&
              part.tools.map((entry) => (
                <div className="session-tool-slot" key={toolEvidenceKey(entry)}>
                  <ToolSummary
                    tool={entry}
                    turn={turn}
                    sessionId={sessionId}
                    limits={limits}
                    renderTool={renderTool}
                  />
                  {moreTool(entry)}
                </div>
              ))}
            {detail === 'results' &&
              tool &&
              part.tools.some((entry) => toolEvidenceKey(entry) === toolEvidenceKey(tool)) && (
                <div className="session-tool-slot">
                  <ToolSummary
                    tool={tool}
                    turn={turn}
                    sessionId={sessionId}
                    limits={limits}
                    renderTool={renderTool}
                  />
                  {moreTool(tool)}
                </div>
              )}
          </section>
        ),
      )}
      {turn.outcome && <BodyText body={turn.outcome.body} />}
    </>
  )
}

function ToolSummary({
  tool,
  turn,
  sessionId,
  limits,
  renderTool,
}: {
  tool: WebTimelineToolAttempt
  turn: TranscriptTurn
  sessionId: string
  limits: SessionTranscriptLimits
  renderTool?: SessionTranscriptTextProps['renderTool']
}) {
  const evidence = tool.evidence.type === 'physical_attempt' ? tool.evidence : null
  const event = turn.events.findLast(
    (item) =>
      item.body.type === 'tool_batch' &&
      item.body.tools.some((entry) => toolEvidenceKey(entry) === toolEvidenceKey(tool)),
  )
  const member =
    event?.body.type === 'tool_batch'
      ? (event.body.projected_member_index ?? 0) +
        event.body.tools.findIndex((entry) => toolEvidenceKey(entry) === toolEvidenceKey(tool))
      : 0
  const field = evidence?.failure_present ? 'tool_failure' : 'tool_result'
  const needsOutput = Boolean(
    event &&
      evidence &&
      ((evidence.result_present && !evidence.result) ||
        (evidence.failure_present && !evidence.failure)),
  )
  const output = useQuery({
    queryKey: ['production', 'tool-summary', sessionId, event?.address, member, field, limits],
    enabled: needsOutput,
    queryFn: ({ signal }) =>
      readSessionTranscript(
        sessionId,
        event?.address.event_sequence ?? '',
        event?.address.event_sequence ?? '',
        {
          type: 'more_body',
          body: {
            address: event?.address ?? { event_sequence: '1' },
            field,
            member_index: member,
            offset_bytes: '0',
          },
        },
        limits,
        signal,
        event ? { items: [event] } : undefined,
      ),
    gcTime: 0,
  })
  const body = output.data?.items[0]?.body
  const returned =
    body?.type === 'tool_batch'
      ? body.tools.find((entry) => toolEvidenceKey(entry) === toolEvidenceKey(tool))?.evidence
      : null
  const loaded = returned?.type === 'physical_attempt' ? returned : null
  const result = evidence?.result ?? loaded?.result
  const failure = evidence?.failure ?? loaded?.failure
  return (
    <section aria-label={`${tool.tool_name} details`}>
      {renderTool ? (
        renderTool({ ...tool, evidence: loaded ?? tool.evidence }, 'condensed')
      ) : (
        <>
          <strong>{tool.tool_name}</strong>
          <small>Tool summaries</small>
          {tool.arguments && <ToolText label="Arguments" excerpt={tool.arguments} />}
          {result && <ToolText label="Output" excerpt={result} />}
          {failure && <ToolText label="Failure" excerpt={failure} />}
        </>
      )}
      {needsOutput && output.isPending && <small role="status">Loading output…</small>}
      {needsOutput && output.isError && <small role="alert">Output could not be loaded.</small>}
    </section>
  )
}

function EventDetail({
  event,
  sessionId,
  turnId,
  limits,
  renderTool,
  target,
  continuation,
  onContinuation,
}: {
  event: WebSessionTimelineDetail
  sessionId: string
  turnId: string | null
  limits: SessionTranscriptLimits
  renderTool?: SessionTranscriptTextProps['renderTool']
  target: boolean
  continuation?: EventContinuationState
  onContinuation: (state: EventContinuationState) => void
}) {
  const sequence = event.address.event_sequence
  const cursor = continuation?.cursor ?? null
  const earlier = continuation?.earlier ?? []
  const element = useRef<HTMLDivElement>(null)
  const detail = useQuery({
    queryKey: ['production', 'transcript-event', sessionId, turnId, sequence, cursor, limits],
    queryFn: ({ signal }) =>
      turnId
        ? readTurnTranscript(
            sessionId,
            turnId,
            cursor ?? { type: 'more_at', address: event.address },
            limits,
            signal,
            continuation?.previous,
          )
        : readSessionTranscript(
            sessionId,
            sequence,
            sequence,
            cursor,
            limits,
            signal,
            continuation?.previous,
          ),
    gcTime: 0,
    initialData: continuation?.current?.page,
    staleTime: continuation?.current ? Number.POSITIVE_INFINITY : 0,
  })
  const item = detail.data?.items[0]
  const matches = item?.address.event_sequence === sequence && item.kind === event.kind
  const next =
    matches && detail.data?.continuation?.type === 'more_body' ? detail.data.continuation : null
  useEffect(() => {
    if (!continuation || !detail.data || !matches || continuation.current?.page === detail.data)
      return
    onContinuation({
      cursor,
      earlier,
      previous: continuation?.previous,
      current: { key: JSON.stringify(cursor), page: detail.data },
    })
  }, [detail.data, matches, cursor, earlier, continuation, onContinuation])
  useEffect(() => {
    if (!target || !matches) return
    const frame = requestAnimationFrame(() => {
      element.current?.scrollIntoView({ block: 'center' })
      element.current?.focus({ preventScroll: true })
    })
    return () => cancelAnimationFrame(frame)
  }, [target, matches])
  return (
    <div ref={element} tabIndex={-1} className="session-turn-event" data-event-sequence={sequence}>
      <header>
        <a href={`/sessions?workspace=true&session=${sessionId}&around=${sequence}`}>
          {enumLabel(event.kind)}
        </a>
      </header>
      {[...earlier, ...(matches ? [{ key: JSON.stringify(cursor), item }] : [])].map(
        ({ key, item }) => (
          <div key={key}>
            {item.body.type === 'tool_batch' && renderTool ? (
              <>
                {item.body.tools.map((tool) => (
                  <div key={tool.request_id}>{renderTool(tool, 'full')}</div>
                ))}
                {item.body.goal_events.length > 0 && (
                  <section aria-label="Goal events">
                    {item.body.goal_events.map((event) => (
                      <GoalEventDetail key={`${event.generation}:${event.type}`} event={event} />
                    ))}
                  </section>
                )}
              </>
            ) : (
              <BodyText body={item.body} />
            )}
            {!['user_input', 'model_call', 'tool_batch'].includes(item.body.type) && (
              <pre className="session-event-facts">{JSON.stringify(item.body, null, 2)}</pre>
            )}
          </div>
        ),
      )}
      {detail.isPending ? (
        <p role="status">Loading details…</p>
      ) : detail.isError || !matches ? (
        <p role="alert">
          Details could not be loaded.{' '}
          <button type="button" onClick={() => void detail.refetch()}>
            Retry details
          </button>
        </p>
      ) : (
        next && (
          <button
            type="button"
            onClick={() => {
              if (!detail.data) return
              onContinuation({
                cursor: next,
                earlier: [...earlier, { key: JSON.stringify(cursor), item }],
                previous: detail.data,
              })
            }}
          >
            Continue reading
          </button>
        )
      )}
    </div>
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
  state,
  onChange,
}: {
  sessionId: string
  page: WebSessionTimelineDetailPage
  limits: SessionTranscriptLimits
  state?: ContinuedEventState
  onChange: (state?: ContinuedEventState) => void
}) {
  const cursor = state?.cursor ?? page.continuation ?? null
  const earlier = state?.earlier ?? []
  const sequence = page.items.at(-1)?.address.event_sequence ?? continuationSequence(page)
  const detail = useQuery({
    queryKey: ['production', 'transcript-continuation', sessionId, sequence, cursor, limits],
    enabled: Boolean(state),
    queryFn: ({ signal }) =>
      readSessionTranscript(
        sessionId,
        sequence,
        sequence,
        cursor,
        limits,
        signal,
        state?.previous ?? page,
      ),
    gcTime: 0,
    initialData: state?.current,
    staleTime: state?.current ? Number.POSITIVE_INFINITY : 0,
  })
  useEffect(() => {
    if (!state || !detail.data || state.current === detail.data) return
    onChange({ ...state, current: detail.data })
  }, [state, detail.data, onChange])
  if (!state)
    return (
      <button
        type="button"
        onClick={() => onChange({ sequence, cursor, earlier: [], previous: page })}
      >
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
      {[
        ...earlier,
        ...(detail.data ? [{ key: JSON.stringify(cursor), items: detail.data.items }] : []),
      ].map(({ key, items }) => (
        <div key={key}>
          {items.map((item) => (
            <div key={conversationEntryKey(item)}>
              <BodyText body={item.body} />
            </div>
          ))}
        </div>
      ))}
      {detail.data?.continuation && (
        <button
          type="button"
          onClick={() => {
            if (detail.data) {
              const items = detail.data.items
              onChange({
                sequence,
                earlier: [...earlier, { key: JSON.stringify(cursor), items }],
                previous: detail.data,
                cursor: detail.data.continuation ?? null,
              })
            }
          }}
        >
          Continue reading
        </button>
      )}
      <button type="button" onClick={() => onChange(undefined)}>
        Close details
      </button>
    </section>
  )
}
