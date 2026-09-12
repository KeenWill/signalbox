import { useInfiniteQuery, useQuery } from '@tanstack/react-query'
import { useLocation } from '@tanstack/react-router'
import { type ReactNode, useEffect, useMemo, useRef, useState } from 'react'
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
import { conversationEntryKey } from './session-timeline/conversation'
import type { SessionWindowAnchor } from './session-timeline/model'
import { readTranscriptWindow, TRANSCRIPT_RETAINED_WINDOWS } from './session-timeline/transcript'
import { readTurnTranscript } from './session-timeline/turn-detail'
import { groupTranscriptTurns, type TranscriptTurn } from './session-timeline/turns'
import { type DetailMode, store, useAppDispatch, useAppSelector } from './state'
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
  const search = useLocation({ select: (location) => location.searchStr })
  const params = new URLSearchParams(search)
  const address = props.eventSequence ?? params.get('around') ?? undefined
  const eventSequence = address && /^[1-9]\d{0,19}$/.test(address) ? address : undefined
  const requestedTurn = props.turnId ?? params.get('turn') ?? undefined
  const turnId =
    requestedTurn &&
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(requestedTurn)
      ? requestedTurn
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
      key={`${props.sessionId}:${target ?? ''}:${turnId ?? ''}`}
      {...props}
      eventSequence={target}
      turnId={turnId}
    />
  )
}

function TranscriptWindow({
  sessionId,
  observed,
  limits,
  eventSequence,
  renderTool,
  turnId: requestedTurn,
  context,
}: SessionTranscriptTextProps) {
  const detail = useAppSelector((state) => state.app.detail)
  const dispatch = useAppDispatch()
  const [turnModes, setTurnModes] = useState<Record<string, DetailMode>>({})
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
        (turn) =>
          detail === 'full' ||
          turnModes[turn.id] === 'full' ||
          turn.events.some((event) => event.address.event_sequence === eventSequence) ||
          turn.messages.length > 0 ||
          turn.result ||
          turn.tools.length > 0,
      ),
    [entries, detail, turnModes, eventSequence],
  )
  const ids = useMemo(() => turns.map((turn) => turn.id), [turns])
  useEffect(() => {
    const loaded = new Set(entries.map((entry) => groupTranscriptTurns([entry])[0]?.id))
    setTurnModes((current) =>
      Object.keys(current).some((id) => !loaded.has(id))
        ? Object.fromEntries(Object.entries(current).filter(([id]) => loaded.has(id)))
        : current,
    )
  }, [entries])
  return (
    <section className="session-transcript-text" aria-label="Transcript text">
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
              onChange={() => {
                setTurnModes({})
                invokeCommand(`detail.${mode}`, commandContext)
              }}
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
              <TurnContent
                turn={turn}
                detail={
                  turnModes[turn.id] ??
                  (detail === 'full' ||
                  requestedTurn === turn.turnId ||
                  turn.events.some((event) => event.address.event_sequence === eventSequence)
                    ? 'full'
                    : detail)
                }
                renderTool={renderTool}
                sessionId={sessionId}
                limits={limits}
                target={eventSequence}
                onExpand={(mode) => setTurnModes((current) => ({ ...current, [turn.id]: mode }))}
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
  renderTool,
  detail,
  sessionId,
  limits,
  target,
  onExpand,
}: {
  turn: TranscriptTurn
  renderTool?: SessionTranscriptTextProps['renderTool']
  detail: DetailMode
  sessionId: string
  limits: SessionTranscriptLimits
  target?: string
  onExpand: (mode: DetailMode) => void
}) {
  const [openTool, setOpenTool] = useState<string | null>(null)
  const tool = turn.tools.find((entry) => entry.request_id === openTool)
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
          <button type="button" onClick={() => onExpand(detail === 'full' ? 'results' : 'full')}>
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
      {detail === 'condensed' &&
        turn.tools.map((entry) => (
          <div className="session-tool-slot" key={entry.request_id}>
            {renderTool ? (
              renderTool(entry, 'condensed')
            ) : (
              <ToolSummary tool={entry} turn={turn} sessionId={sessionId} limits={limits} />
            )}
          </div>
        ))}
      {detail === 'results' && tool && (
        <div className="session-tool-slot">
          {renderTool ? (
            renderTool(tool, 'condensed')
          ) : (
            <ToolSummary tool={tool} turn={turn} sessionId={sessionId} limits={limits} />
          )}
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

function ToolSummary({
  tool,
  turn,
  sessionId,
  limits,
}: {
  tool: WebTimelineToolAttempt
  turn: TranscriptTurn
  sessionId: string
  limits: SessionTranscriptLimits
}) {
  const evidence = tool.evidence.type === 'physical_attempt' ? tool.evidence : null
  const event = turn.events.findLast(
    (item) =>
      item.body.type === 'tool_batch' &&
      item.body.tools.some((entry) => entry.request_id === tool.request_id),
  )
  const member =
    event?.body.type === 'tool_batch'
      ? (event.body.projected_member_index ?? 0) +
        event.body.tools.findIndex((entry) => entry.request_id === tool.request_id)
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
      ),
    gcTime: 0,
  })
  const body = output.data?.items[0]?.body
  const returned =
    body?.type === 'tool_batch'
      ? body.tools.find((entry) => entry.request_id === tool.request_id)?.evidence
      : null
  const loaded = returned?.type === 'physical_attempt' ? returned : null
  return (
    <section aria-label={`${tool.tool_name} details`}>
      <strong>{tool.tool_name}</strong>
      {tool.arguments && <p className="session-tool-summary">{tool.arguments.text}</p>}
      {(evidence?.result ?? loaded?.result) && (
        <p className="session-tool-summary">{(evidence?.result ?? loaded?.result)?.text}</p>
      )}
      {(evidence?.failure ?? loaded?.failure) && (
        <p className="session-tool-summary">{(evidence?.failure ?? loaded?.failure)?.text}</p>
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
}: {
  event: WebSessionTimelineDetail
  sessionId: string
  turnId: string | null
  limits: SessionTranscriptLimits
  renderTool?: SessionTranscriptTextProps['renderTool']
  target: boolean
}) {
  const sequence = event.address.event_sequence
  const [cursor, setCursor] = useState<WebTimelineDetailContinuation | null>(null)
  const previous = useRef<WebSessionTimelineDetailPage | undefined>(undefined)
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
            previous.current,
          )
        : readSessionTranscript(
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
  const item = detail.data?.items[0]
  const matches = item?.address.event_sequence === sequence && item.kind === event.kind
  const next =
    matches && detail.data?.continuation?.type === 'more_body' ? detail.data.continuation : null
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
        <>
          {item.body.type === 'tool_batch' && renderTool ? (
            item.body.tools.map((tool) => (
              <div key={tool.request_id}>{renderTool(tool, 'full')}</div>
            ))
          ) : (
            <BodyText body={item.body} />
          )}
          {!['user_input', 'model_call', 'tool_batch'].includes(item.body.type) && (
            <pre className="session-event-facts">{JSON.stringify(item.body, null, 2)}</pre>
          )}
          {next && (
            <button
              type="button"
              onClick={() => {
                previous.current = detail.data
                setCursor(next)
              }}
            >
              Continue reading
            </button>
          )}
        </>
      )}
    </div>
  )
}
