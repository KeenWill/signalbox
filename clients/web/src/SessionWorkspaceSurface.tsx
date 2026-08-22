import { useQuery } from '@tanstack/react-query'
import { ChevronDown, ChevronRight, Radio, SkipBack, SkipForward } from 'lucide-react'
import {
  type FormEvent,
  type KeyboardEvent,
  type RefObject,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react'
import { invokeCommand } from './commands'
import type { WebSessionTimelineWindow } from './generated/web-contract.mjs'
import {
  BoundedSessionHistory,
  HttpSessionTimelineSource,
  type SessionWindowAnchor,
} from './session-timeline/model'
import { actions, selectApp, store, useAppDispatch, useAppSelector } from './state'

const SESSION_ID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i
const NATIVE_SESSION_ID_PATTERN = String.raw`\s*[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\s*`
const SESSION_WINDOW_ITEMS = 80
const SESSION_WINDOW_BYTES = 64 * 1024

export const isCanonicalSessionId = (value: string): boolean => SESSION_ID_PATTERN.test(value)

export const visibleSessionItems = (
  items: WebSessionTimelineWindow['items'],
  detail: 'full' | 'condensed' | 'results',
) =>
  detail === 'results'
    ? items.filter((item) =>
        [
          'input_accepted',
          'turn_completed',
          'turn_failed',
          'turn_refused',
          'turn_cancelled',
        ].includes(item.kind),
      )
    : items

export function SessionWorkspaceSurface({
  onTimelineIds,
  onTimelineWindowAvailable,
  onWindowRequestConsumed,
  timelineRef,
  windowRequest,
}: {
  onTimelineIds: (ids: readonly string[]) => void
  onTimelineWindowAvailable: (available: boolean) => void
  onWindowRequestConsumed: () => void
  timelineRef: RefObject<HTMLDivElement | null>
  windowRequest: { anchor: 'first' | 'latest'; attempt: number } | null
}) {
  const dispatch = useAppDispatch()
  const app = useAppSelector(selectApp)
  const [draftId, setDraftId] = useState('')
  const [sessionId, setSessionId] = useState<string | null>(null)
  const [manualAnchor, setManualAnchor] = useState<SessionWindowAnchor | null>(null)
  const [openingPosition, setOpeningPosition] = useState<string | undefined>()
  const [attempt, setAttempt] = useState(0)
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set())
  const rowRefs = useRef(new Map<string, HTMLDivElement>())
  const session = useQuery({
    queryKey: [
      'production',
      'session-workspace',
      sessionId,
      manualAnchor,
      openingPosition,
      attempt,
    ],
    queryFn: async ({ signal }) => {
      const source = await HttpSessionTimelineSource.connect(window.fetch.bind(window))
      const history = new BoundedSessionHistory(sessionId ?? '', source)
      const descriptor = await history.describe(signal)
      const active = BigInt(descriptor.work.active_turn_count) !== BigInt(0)
      const anchor: SessionWindowAnchor =
        manualAnchor ??
        (!active && openingPosition
          ? { kind: 'around', eventSequence: openingPosition }
          : { kind: 'latest' })
      const timelineWindow = await history.load(
        anchor,
        { maxItems: SESSION_WINDOW_ITEMS, maxBytes: SESSION_WINDOW_BYTES },
        signal,
      )
      return { active, anchor, descriptor, window: timelineWindow }
    },
    enabled: sessionId !== null,
  })
  const items = useMemo(
    () => visibleSessionItems(session.data?.window.items ?? [], app.detail),
    [app.detail, session.data?.window.items],
  )
  const timelineIds = useMemo(() => items.map((item) => item.address.event_sequence), [items])

  useEffect(() => onTimelineIds(timelineIds), [onTimelineIds, timelineIds])
  useEffect(() => () => onTimelineIds([]), [onTimelineIds])
  useEffect(
    () => onTimelineWindowAvailable(session.data !== undefined),
    [onTimelineWindowAvailable, session.data],
  )
  useEffect(() => () => onTimelineWindowAvailable(false), [onTimelineWindowAvailable])
  useEffect(() => {
    if (windowRequest === null || sessionId === null) return
    setManualAnchor({ kind: windowRequest.anchor })
    setAttempt((current) => current + 1)
    onWindowRequestConsumed()
  }, [onWindowRequestConsumed, sessionId, windowRequest])
  useEffect(() => {
    if (sessionId !== null && app.selectedTimeline !== null) {
      dispatch(
        actions.logicalPositionRecorded({
          sessionId,
          position: app.selectedTimeline,
        }),
      )
    }
  }, [app.selectedTimeline, dispatch, sessionId])
  useEffect(() => {
    if (app.selectedTimeline !== null) {
      rowRefs.current.get(app.selectedTimeline)?.scrollIntoView({ block: 'nearest' })
    }
  }, [app.selectedTimeline])

  const openSession = (event: FormEvent) => {
    event.preventDefault()
    const candidate = draftId.trim().toLowerCase()
    if (!isCanonicalSessionId(candidate)) return
    setOpeningPosition(app.lastLogicalPositions[candidate])
    setManualAnchor(null)
    setAttempt((current) => current + 1)
    setExpanded(new Set())
    dispatch(actions.timelineSelected(null))
    setSessionId(candidate)
  }
  const selected = app.selectedTimeline
  const loadWindow = (anchor: 'first' | 'latest') => {
    setManualAnchor({ kind: anchor })
    setAttempt((current) => current + 1)
  }
  const select = (eventSequence: string) => {
    dispatch(actions.timelineSelected(eventSequence))
  }
  const handleTimelineKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const command = {
      ArrowDown: 'selection.next',
      ArrowUp: 'selection.previous',
      Home: 'selection.first',
      End: 'selection.last',
    }[event.key] as
      | 'selection.next'
      | 'selection.previous'
      | 'selection.first'
      | 'selection.last'
      | undefined
    if (!command) return
    event.preventDefault()
    invokeCommand(command, {
      dispatch,
      getState: store.getState,
      timelineIds,
      timelineWindowAvailable: session.data !== undefined,
      focusTimeline: () => timelineRef.current?.focus(),
      loadTimelineWindow: loadWindow,
    })
  }
  const toggleExpanded = (eventSequence: string) => {
    const next = new Set(expanded)
    if (next.has(eventSequence)) next.delete(eventSequence)
    else next.add(eventSequence)
    setExpanded(next)
  }

  return (
    <div className="surface-body session-workspace-surface">
      <form className="session-open-form" onSubmit={openSession}>
        <label>
          Exact session ID
          <input
            aria-label="Exact session ID"
            placeholder="00000000-0000-0000-0000-000000000000"
            value={draftId}
            onChange={(event) => setDraftId(event.target.value)}
            pattern={NATIVE_SESSION_ID_PATTERN}
            required
          />
        </label>
        <button type="submit" disabled={!isCanonicalSessionId(draftId.trim())}>
          Open workspace
        </button>
      </form>

      {sessionId === null ? (
        <section className="surface-empty session-entry" aria-labelledby="session-entry-heading">
          <Radio aria-hidden="true" />
          <div>
            <span className="availability-tag ready">Timeline reads available</span>
            <h2 id="session-entry-heading">Open a known session by immutable identity</h2>
            <p>
              This branch provides bounded descriptor and timeline reads, but no session catalog,
              creation operation, or live follow channel. Enter an exact server-issued ID.
            </p>
          </div>
        </section>
      ) : session.isError ? (
        <p className="session-load-state" role="alert">
          The daemon could not provide this bounded session window: {session.error.message}
        </p>
      ) : !session.data ? (
        <p className="session-load-state">Loading descriptor and bounded history…</p>
      ) : (
        <section className="session-workspace" aria-labelledby="session-workspace-heading">
          <header className="session-workspace-header">
            <div>
              <span className="eyebrow">Stable timeline identity</span>
              <h2 id="session-workspace-heading">{sessionId}</h2>
              <p>
                {session.data.active ? 'Active' : 'Inactive'} ·{' '}
                {session.data.anchor.kind === 'first'
                  ? 'opened at first'
                  : session.data.anchor.kind === 'latest'
                    ? 'opened near latest'
                    : 'restored logical position'}
              </p>
            </div>
            <dl className="session-telemetry">
              <div>
                <dt>Items</dt>
                <dd>{session.data.descriptor.sizes.item_count}</dd>
              </div>
              <div>
                <dt>Active</dt>
                <dd>{session.data.descriptor.work.active_turn_count}</dd>
              </div>
              <div>
                <dt>Queued</dt>
                <dd>{session.data.descriptor.work.queued_turn_count}</dd>
              </div>
              <div>
                <dt>Observed</dt>
                <dd>{session.data.descriptor.observed_through}</dd>
              </div>
            </dl>
          </header>
          <div className="session-window-controls" role="toolbar" aria-label="Timeline window">
            <button type="button" onClick={() => loadWindow('first')}>
              <SkipBack aria-hidden="true" /> First <kbd>gg</kbd>
            </button>
            <button type="button" onClick={() => loadWindow('latest')}>
              <SkipForward aria-hidden="true" /> Latest <kbd>G</kbd>
            </button>
            <span>
              {session.data.window.items.length} bounded items ·{' '}
              {session.data.window.projected_structured_bytes} B
            </span>
          </div>
          <div
            className="session-timeline"
            aria-label="Session timeline"
            aria-activedescendant={
              selected !== null && timelineIds.includes(selected)
                ? `session-timeline-option-${selected}`
                : undefined
            }
            ref={timelineRef}
            role="listbox"
            tabIndex={-1}
            onKeyDown={handleTimelineKeyDown}
          >
            {items.map((item) => {
              const id = item.address.event_sequence
              const isExpanded = expanded.has(id)
              return (
                <div
                  id={`session-timeline-option-${id}`}
                  key={id}
                  role="option"
                  aria-selected={selected === id}
                  tabIndex={-1}
                  ref={(node) => {
                    if (node) rowRefs.current.set(id, node)
                    else rowRefs.current.delete(id)
                  }}
                  className={selected === id ? 'selected' : undefined}
                >
                  <button
                    type="button"
                    className="session-item-summary"
                    aria-expanded={isExpanded}
                    onClick={() => {
                      select(id)
                      toggleExpanded(id)
                    }}
                  >
                    {isExpanded ? (
                      <ChevronDown aria-hidden="true" />
                    ) : (
                      <ChevronRight aria-hidden="true" />
                    )}
                    <span className="session-address">{id}</span>
                    <strong>{item.kind.replaceAll('_', ' ')}</strong>
                    <small>{item.projected_structured_bytes} B</small>
                  </button>
                  {isExpanded && (
                    <dl className="session-item-detail">
                      <div>
                        <dt>Address</dt>
                        <dd>
                          {sessionId}:{id}
                        </dd>
                      </div>
                      <div>
                        <dt>Projection</dt>
                        <dd>Header only; rich event detail is not exposed</dd>
                      </div>
                    </dl>
                  )}
                </div>
              )
            })}
          </div>
        </section>
      )}
    </div>
  )
}
