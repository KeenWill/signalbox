import { useQuery } from '@tanstack/react-query'
import { ChevronDown, ChevronRight, Radio, SkipBack, SkipForward } from 'lucide-react'
import { type FormEvent, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { WebContractBootstrap, WebSessionTimelineWindow } from './generated/web-contract.mjs'
import type { ProductCommandId } from './productCommands'
import { SessionItemDetail } from './SessionItemDetail'
import {
  BoundedSessionHistory,
  HttpSessionTimelineSource,
  type SessionWindowAnchor,
} from './session-timeline/model'
import { actions, selectApp, useAppDispatch, useAppSelector } from './state'

const SESSION_ID_PATTERN =
  /^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$/
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
          'turn_reconciliation_required',
          'model_call_transition',
        ].includes(item.kind),
      )
    : detail === 'condensed'
      ? items.filter((item) => item.kind !== 'model_call_transition')
      : items

export function SessionWorkspaceSurface({
  bootstrap,
  onTimelineIds,
  onTimelineWindowNavigation,
  onTimelineWindowAvailability,
  onTimelineWindowCommand,
  onEmptyTimelineFocus,
}: {
  bootstrap: WebContractBootstrap | undefined
  onTimelineIds: (ids: readonly string[]) => void
  onTimelineWindowNavigation: (navigate: (anchor: 'first' | 'latest') => void) => void
  onTimelineWindowAvailability: (available: boolean) => void
  onTimelineWindowCommand: (
    command: Extract<ProductCommandId, 'selection.first' | 'selection.last'>,
  ) => void
  onEmptyTimelineFocus: () => void
}) {
  const dispatch = useAppDispatch()
  const app = useAppSelector(selectApp)
  const [draftId, setDraftId] = useState('')
  const [sessionId, setSessionId] = useState<string | null>(null)
  const [restoreAnchor, setRestoreAnchor] = useState<SessionWindowAnchor | null>(null)
  const [manualAnchor, setManualAnchor] = useState<SessionWindowAnchor | null>(null)
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set())
  const retryRef = useRef<HTMLButtonElement>(null)
  const firstWindowRef = useRef<HTMLButtonElement>(null)
  const latestWindowRef = useRef<HTMLButtonElement>(null)
  const pendingWindowFocusRef = useRef<'first' | 'latest' | null>(null)
  const session = useQuery({
    queryKey: ['production', 'session-workspace', sessionId, manualAnchor, restoreAnchor],
    queryFn: async ({ signal }) => {
      if (!bootstrap) throw new TypeError('web contract bootstrap is unavailable')
      const source = HttpSessionTimelineSource.fromBootstrap(bootstrap, window.fetch.bind(window))
      const history = new BoundedSessionHistory(sessionId ?? '', source)
      const descriptor = await history.describe(signal)
      const active = BigInt(descriptor.work.active_turn_count) !== BigInt(0)
      const anchor: SessionWindowAnchor =
        manualAnchor ?? (!active && restoreAnchor ? restoreAnchor : { kind: 'latest' })
      const timelineWindow = await history.load(
        anchor,
        { maxItems: SESSION_WINDOW_ITEMS, maxBytes: SESSION_WINDOW_BYTES },
        signal,
      )
      return { active, anchor, descriptor, source, window: timelineWindow }
    },
    enabled: sessionId !== null && bootstrap?.capabilities.bounded_session_timeline === true,
    gcTime: 0,
    placeholderData: (previous) => previous,
  })
  const items = useMemo(
    () => visibleSessionItems(session.data?.window.items ?? [], app.detail),
    [app.detail, session.data?.window.items],
  )
  const timelineIds = useMemo(() => items.map((item) => item.address.event_sequence), [items])

  useEffect(() => {
    if (session.isError) retryRef.current?.focus()
  }, [session.isError])

  useEffect(() => onTimelineIds(timelineIds), [onTimelineIds, timelineIds])
  useEffect(() => () => onTimelineIds([]), [onTimelineIds])
  useEffect(() => {
    onTimelineWindowAvailability(session.data !== undefined)
    return () => onTimelineWindowAvailability(false)
  }, [onTimelineWindowAvailability, session.data])
  useEffect(() => {
    const windowIds = new Set(
      session.data?.window.items.map((item) => item.address.event_sequence) ?? [],
    )
    setExpanded((current) => {
      const retained = new Set([...current].filter((id) => windowIds.has(id)))
      return retained.size === current.size ? current : retained
    })
  }, [session.data?.window.items])
  const activeAnchorKind = session.data?.anchor.kind
  const refetchSession = session.refetch
  const navigateTimelineWindow = useCallback(
    (anchor: 'first' | 'latest') => {
      if (activeAnchorKind === anchor) void refetchSession()
      else setManualAnchor({ kind: anchor })
    },
    [activeAnchorKind, refetchSession],
  )
  useEffect(() => {
    const pending = pendingWindowFocusRef.current
    if (!pending) return
    const frame = window.requestAnimationFrame(() => {
      const control = pending === 'first' ? firstWindowRef.current : latestWindowRef.current
      control?.focus()
      if (!session.isFetching) pendingWindowFocusRef.current = null
    })
    return () => window.cancelAnimationFrame(frame)
  }, [session.isFetching])
  useEffect(() => {
    onTimelineWindowNavigation(navigateTimelineWindow)
    return () => onTimelineWindowNavigation(() => {})
  }, [navigateTimelineWindow, onTimelineWindowNavigation])

  const openSession = (event: FormEvent) => {
    event.preventDefault()
    const candidate = draftId.trim().toLowerCase()
    if (!isCanonicalSessionId(candidate)) return
    setManualAnchor(null)
    const remembered = app.lastLogicalPositions[candidate]
    setRestoreAnchor(remembered ? { kind: 'around', eventSequence: remembered } : null)
    setExpanded(new Set())
    dispatch(actions.timelineSelected(null))
    if (candidate === sessionId) void session.refetch()
    else setSessionId(candidate)
  }
  const selected = app.selectedTimeline
  const timelineAvailable = bootstrap?.capabilities.bounded_session_timeline === true
  const detailAvailable = bootstrap?.capabilities.bounded_session_timeline_detail === true
  const select = (eventSequence: string) => {
    dispatch(actions.timelineSelected(eventSequence))
  }
  useEffect(() => {
    if (!session.data) return
    if (timelineIds.length === 0) {
      if (selected !== null) dispatch(actions.timelineSelected(null))
      onEmptyTimelineFocus()
      return
    }
    if (selected !== null && timelineIds.includes(selected)) return
    const next = session.data.anchor.kind === 'latest' ? timelineIds.at(-1) : timelineIds[0]
    if (next) dispatch(actions.timelineSelected(next))
  }, [dispatch, onEmptyTimelineFocus, selected, session.data, timelineIds])
  useEffect(() => {
    if (sessionId !== null && selected !== null && timelineIds.includes(selected)) {
      dispatch(actions.logicalPositionRecorded({ sessionId, position: selected }))
    }
  }, [dispatch, selected, sessionId, timelineIds])
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
            onChange={(event) => setDraftId(event.target.value.trim())}
            pattern={SESSION_ID_PATTERN.source}
            required
          />
        </label>
        <button
          type="submit"
          disabled={!isCanonicalSessionId(draftId.trim()) || !timelineAvailable}
        >
          Open workspace
        </button>
      </form>

      {sessionId === null ? (
        <section className="surface-empty session-entry" aria-labelledby="session-entry-heading">
          <Radio aria-hidden="true" />
          <div>
            <span className={`availability-tag ${timelineAvailable ? 'ready' : ''}`}>
              {timelineAvailable ? 'Timeline reads available' : 'Timeline reads unavailable'}
            </span>
            <h2 id="session-entry-heading">Open a known session by immutable identity</h2>
            <p>
              {timelineAvailable
                ? `This daemon provides bounded descriptor and timeline reads${
                    detailAvailable ? ' with typed item detail' : ''
                  }, but no session catalog, creation operation, or live follow channel. Enter an exact server-issued ID.`
                : 'The current daemon contract does not advertise bounded session timeline reads.'}
            </p>
          </div>
        </section>
      ) : session.isError ? (
        <div className="session-load-state" role="alert">
          <p>The daemon could not provide this bounded session window: {session.error.message}</p>
          <button ref={retryRef} type="button" onClick={() => void session.refetch()}>
            Retry
          </button>
        </div>
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
                    : session.data.anchor.kind === 'around'
                      ? 'restored logical position'
                      : `opened ${session.data.anchor.kind} selected position`}
              </p>
            </div>
            <dl className="session-telemetry" aria-label="Session telemetry">
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
            <button
              ref={firstWindowRef}
              type="button"
              onClick={() => {
                pendingWindowFocusRef.current = 'first'
                onTimelineWindowCommand('selection.first')
              }}
            >
              <SkipBack aria-hidden="true" /> First <kbd>gg</kbd>
            </button>
            <button
              ref={latestWindowRef}
              type="button"
              onClick={() => {
                pendingWindowFocusRef.current = 'latest'
                onTimelineWindowCommand('selection.last')
              }}
            >
              <SkipForward aria-hidden="true" /> Latest <kbd>G</kbd>
            </button>
            <span>
              {session.data.window.items.length} bounded items ·{' '}
              {session.data.window.projected_structured_bytes} B
            </span>
          </div>
          <ol className="session-timeline" aria-label="Session timeline">
            {items.map((item) => {
              const id = item.address.event_sequence
              const isExpanded = expanded.has(id)
              return (
                <li key={id} className={selected === id ? 'selected' : undefined}>
                  <button
                    type="button"
                    className={
                      detailAvailable ? 'session-item-summary' : 'no-detail session-item-summary'
                    }
                    data-timeline-id={id}
                    aria-expanded={detailAvailable ? isExpanded : undefined}
                    onClick={() => {
                      select(id)
                      if (detailAvailable) toggleExpanded(id)
                    }}
                  >
                    {detailAvailable &&
                      (isExpanded ? (
                        <ChevronDown aria-hidden="true" />
                      ) : (
                        <ChevronRight aria-hidden="true" />
                      ))}
                    <span className="session-address">{id}</span>
                    <strong>{item.kind.replaceAll('_', ' ')}</strong>
                    <small>{item.projected_structured_bytes} B</small>
                  </button>
                  {isExpanded && detailAvailable && (
                    <SessionItemDetail
                      source={session.data.source}
                      sessionId={sessionId}
                      item={item}
                    />
                  )}
                </li>
              )
            })}
          </ol>
        </section>
      )}
    </div>
  )
}
