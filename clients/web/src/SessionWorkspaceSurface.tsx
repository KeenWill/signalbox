import { useQuery } from '@tanstack/react-query'
import { ChevronDown, ChevronRight, Radio, SkipBack, SkipForward } from 'lucide-react'
import {
  type FormEvent,
  type KeyboardEvent,
  type RefObject,
  useCallback,
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
type TimelineCapability = 'checking' | 'available' | 'unavailable'
export interface SessionSelectionEvidence {
  sessionId: string
  eventSequence: string
  kind: string
  projectedStructuredBytes: number
}

export const isCanonicalSessionId = (value: string): boolean => SESSION_ID_PATTERN.test(value)
export const sessionWorkspaceQueryKey = (sessionId: string | null) =>
  ['production', 'session-workspace', sessionId] as const

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
          'turn_reconciliation_required',
        ].includes(item.kind),
      )
    : items

export const boundarySessionItemId = (
  items: WebSessionTimelineWindow['items'],
  detail: 'full' | 'condensed' | 'results',
  anchor: 'first' | 'latest',
): string | null => {
  const visible = visibleSessionItems(items, detail)
  const boundary = anchor === 'first' ? visible[0] : visible[visible.length - 1]
  return boundary?.address.event_sequence ?? null
}

export const pruneExpandedSessionItems = (
  expanded: ReadonlySet<string>,
  items: WebSessionTimelineWindow['items'],
): ReadonlySet<string> => {
  const loadedIds = new Set(items.map((item) => item.address.event_sequence))
  const next = new Set([...expanded].filter((id) => loadedIds.has(id)))
  return next.size === expanded.size ? expanded : next
}

export function SessionWorkspaceSurface({
  onSelectionEvidence,
  onTimelineIds,
  onTimelineWindowAvailable,
  onWindowRequestConsumed,
  timelineCapability,
  timelineRef,
  windowRequest,
}: {
  onSelectionEvidence: (evidence: SessionSelectionEvidence | null) => void
  onTimelineIds: (ids: readonly string[]) => void
  onTimelineWindowAvailable: (available: boolean) => void
  onWindowRequestConsumed: () => void
  timelineCapability: TimelineCapability
  timelineRef: RefObject<HTMLDivElement | null>
  windowRequest: { anchor: 'first' | 'latest'; attempt: number } | null
}) {
  const dispatch = useAppDispatch()
  const app = useAppSelector(selectApp)
  const [draftId, setDraftId] = useState('')
  const [sessionId, setSessionId] = useState<string | null>(null)
  const [openingPosition, setOpeningPosition] = useState<string | undefined>()
  const [refetchRequest, setRefetchRequest] = useState(0)
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set())
  const rowRefs = useRef(new Map<string, HTMLDivElement>())
  const manualAnchorRef = useRef<SessionWindowAnchor | null>(null)
  const handledRefetchRequest = useRef(0)
  const boundaryRequest = useRef(0)
  const session = useQuery({
    queryKey: sessionWorkspaceQueryKey(sessionId),
    queryFn: async ({ signal }) => {
      const source = await HttpSessionTimelineSource.connect(window.fetch.bind(window), signal)
      const history = new BoundedSessionHistory(sessionId ?? '', source)
      const descriptor = await history.describe(signal)
      const active = BigInt(descriptor.work.active_turn_count) !== BigInt(0)
      const anchor: SessionWindowAnchor =
        manualAnchorRef.current ??
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
    enabled: sessionId !== null && timelineCapability === 'available',
  })
  const refetchSession = session.refetch
  const items = useMemo(
    () => visibleSessionItems(session.data?.window.items ?? [], app.detail),
    [app.detail, session.data?.window.items],
  )
  const timelineIds = useMemo(() => items.map((item) => item.address.event_sequence), [items])
  const loadWindow = useCallback(
    async (anchor: 'first' | 'latest') => {
      const request = ++boundaryRequest.current
      manualAnchorRef.current = { kind: anchor }
      const result = await refetchSession()
      if (!result.isSuccess || result.data === undefined || request !== boundaryRequest.current) {
        return
      }
      dispatch(
        actions.timelineSelected(
          boundarySessionItemId(result.data.window.items, store.getState().app.detail, anchor),
        ),
      )
    },
    [dispatch, refetchSession],
  )

  useEffect(() => onTimelineIds(timelineIds), [onTimelineIds, timelineIds])
  useEffect(() => () => onTimelineIds([]), [onTimelineIds])
  useEffect(() => {
    const selectedItem = session.data?.window.items.find(
      (item) => item.address.event_sequence === app.selectedTimeline,
    )
    onSelectionEvidence(
      sessionId !== null && selectedItem !== undefined
        ? {
            sessionId,
            eventSequence: selectedItem.address.event_sequence,
            kind: selectedItem.kind,
            projectedStructuredBytes: selectedItem.projected_structured_bytes,
          }
        : null,
    )
  }, [app.selectedTimeline, onSelectionEvidence, session.data?.window.items, sessionId])
  useEffect(() => () => onSelectionEvidence(null), [onSelectionEvidence])
  useEffect(() => {
    setExpanded((current) => pruneExpandedSessionItems(current, session.data?.window.items ?? []))
  }, [session.data?.window.items])
  useEffect(
    () =>
      onTimelineWindowAvailable(timelineCapability === 'available' && session.data !== undefined),
    [onTimelineWindowAvailable, session.data, timelineCapability],
  )
  useEffect(() => () => onTimelineWindowAvailable(false), [onTimelineWindowAvailable])
  useEffect(() => {
    if (windowRequest === null || sessionId === null) return
    void loadWindow(windowRequest.anchor)
    onWindowRequestConsumed()
  }, [loadWindow, onWindowRequestConsumed, sessionId, windowRequest])
  useEffect(() => {
    if (
      refetchRequest === handledRefetchRequest.current ||
      sessionId === null ||
      timelineCapability !== 'available'
    ) {
      return
    }
    handledRefetchRequest.current = refetchRequest
    void refetchSession()
  }, [refetchRequest, refetchSession, sessionId, timelineCapability])
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
    if (!isCanonicalSessionId(candidate) || timelineCapability !== 'available') return
    const reopeningCurrentSession = candidate === sessionId
    setOpeningPosition(app.lastLogicalPositions[candidate])
    manualAnchorRef.current = null
    setExpanded(new Set())
    dispatch(actions.timelineSelected(null))
    setSessionId(candidate)
    if (reopeningCurrentSession) {
      boundaryRequest.current += 1
      setRefetchRequest((current) => current + 1)
    }
  }
  const selected = app.selectedTimeline
  const select = (eventSequence: string) => {
    dispatch(actions.timelineSelected(eventSequence))
  }
  const handleTimelineKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if ((event.key === 'Enter' || event.key === ' ') && selected !== null) {
      event.preventDefault()
      toggleExpanded(selected)
      return
    }
    if (['j', 'k', 'ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) {
      event.currentTarget.focus()
    }
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
        <button
          type="submit"
          disabled={!isCanonicalSessionId(draftId.trim()) || timelineCapability !== 'available'}
        >
          Open workspace
        </button>
      </form>

      {sessionId === null ? (
        <section className="surface-empty session-entry" aria-labelledby="session-entry-heading">
          <Radio aria-hidden="true" />
          <div>
            <span
              className={`availability-tag ${timelineCapability === 'available' ? 'ready' : ''}`}
            >
              {timelineCapability === 'checking'
                ? 'Checking timeline capability'
                : timelineCapability === 'available'
                  ? 'Timeline reads available'
                  : 'Timeline reads unavailable'}
            </span>
            <h2 id="session-entry-heading">Open a known session by immutable identity</h2>
            <p>
              {timelineCapability === 'available'
                ? 'This branch provides bounded descriptor and timeline reads, but no session catalog, creation operation, or live follow channel. Enter an exact server-issued ID.'
                : 'The validated daemon bootstrap has not authorized bounded session timeline reads. Signalbox will not call or advertise that surface until the capability is available.'}
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
            className={`session-timeline presentation-${app.detail}`}
            aria-label="Session timeline"
            aria-activedescendant={
              selected !== null && timelineIds.includes(selected)
                ? `session-timeline-option-${selected}`
                : undefined
            }
            ref={timelineRef}
            role="listbox"
            tabIndex={0}
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
                  aria-controls={`session-timeline-detail-${id}`}
                  aria-describedby={`session-timeline-disclosure-${id}`}
                  aria-selected={selected === id}
                  tabIndex={-1}
                  ref={(node) => {
                    if (node) rowRefs.current.set(id, node)
                    else rowRefs.current.delete(id)
                  }}
                  className={selected === id ? 'selected' : undefined}
                  onClick={() => {
                    select(id)
                    toggleExpanded(id)
                    timelineRef.current?.focus()
                  }}
                  onKeyDown={(event) => {
                    if (event.key !== 'Enter' && event.key !== ' ') return
                    event.preventDefault()
                    event.stopPropagation()
                    select(id)
                    toggleExpanded(id)
                    timelineRef.current?.focus()
                  }}
                >
                  <span id={`session-timeline-disclosure-${id}`} className="sr-only">
                    {isExpanded ? 'Expanded' : 'Collapsed'}
                  </span>
                  <div className="session-item-summary">
                    {isExpanded ? (
                      <ChevronDown aria-hidden="true" />
                    ) : (
                      <ChevronRight aria-hidden="true" />
                    )}
                    <span className="session-address">{id}</span>
                    <strong>{item.kind.replaceAll('_', ' ')}</strong>
                    <small>{item.projected_structured_bytes} B</small>
                  </div>
                  {isExpanded && (
                    <dl id={`session-timeline-detail-${id}`} className="session-item-detail">
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
