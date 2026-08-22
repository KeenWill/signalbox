import { useQuery } from '@tanstack/react-query'
import { ChevronDown, ChevronRight, Radio, SkipBack, SkipForward } from 'lucide-react'
import { type FormEvent, useCallback, useEffect, useMemo, useState } from 'react'
import type { CommandContext } from './commands'
import type { WebContractBootstrap, WebSessionTimelineWindow } from './generated/web-contract.mjs'
import { SessionItemDetail } from './SessionItemDetail'
import {
  BoundedSessionHistory,
  HttpSessionTimelineSource,
  MAX_CONTRACT_TIMELINE_WINDOW_BYTES,
  MAX_CONTRACT_TIMELINE_WINDOW_ITEMS,
  type SessionWindowAnchor,
} from './session-timeline/model'
import { actions, selectApp, useAppDispatch, useAppSelector } from './state'

const SESSION_ID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i
const SESSION_ID_HTML_PATTERN =
  '[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}'
const SESSION_WINDOW_ITEMS = 80
const SESSION_WINDOW_BYTES = 64 * 1024

export const isCanonicalSessionId = (value: string): boolean => SESSION_ID_PATTERN.test(value)

export const hasUsableSessionTimeline = (bootstrap: WebContractBootstrap | undefined): boolean =>
  bootstrap?.capabilities.bounded_session_timeline === true &&
  bootstrap.capabilities.bounded_session_timeline_detail === true &&
  bootstrap.limits.max_timeline_window_items >= 1 &&
  bootstrap.limits.max_timeline_window_items <= MAX_CONTRACT_TIMELINE_WINDOW_ITEMS &&
  bootstrap.limits.max_timeline_window_bytes >= 256 &&
  bootstrap.limits.max_timeline_window_bytes <= MAX_CONTRACT_TIMELINE_WINDOW_BYTES &&
  bootstrap.limits.max_timeline_detail_items >= 1 &&
  bootstrap.limits.max_timeline_detail_bytes >= 256

export const visibleSessionItems = (
  items: WebSessionTimelineWindow['items'],
  detail: 'full' | 'condensed' | 'results',
) => {
  if (detail === 'results') {
    return items.filter((item) =>
      [
        'input_accepted',
        'model_call_transition',
        'turn_completed',
        'turn_failed',
        'turn_refused',
        'turn_cancelled',
      ].includes(item.kind),
    )
  }
  if (detail === 'condensed') {
    return items.filter((item) =>
      [
        'input_accepted',
        'model_call_transition',
        'tool_batch_transition',
        'tool_approval_decided',
        'goal_turn_retired',
        'turn_failed',
        'turn_completed',
        'turn_refused',
        'turn_cancelled',
        'turn_reconciliation_required',
      ].includes(item.kind),
    )
  }
  return items
}

export const restoredTimelineSelection = (
  restorePosition: string | undefined,
  restored: boolean,
  ids: readonly string[],
): string | undefined =>
  restored && restorePosition && ids.includes(restorePosition) ? restorePosition : undefined

export const projectedTimelineSelection = (
  selected: string | null,
  ids: readonly string[],
): string | null => (selected !== null && !ids.includes(selected) ? (ids[0] ?? null) : selected)

export const sameSessionWindowAnchor = (
  left: SessionWindowAnchor | null,
  right: SessionWindowAnchor | null,
): boolean => {
  if (left === null || right === null) return left === right
  if (left.kind !== right.kind) return false
  return (
    !('eventSequence' in left) ||
    ('eventSequence' in right && left.eventSequence === right.eventSequence)
  )
}

export const timelineArrowTarget = (
  ids: readonly string[],
  selected: string | null,
  key: string,
): string | undefined => {
  if (key !== 'ArrowDown' && key !== 'ArrowUp') return undefined
  const currentIndex = ids.indexOf(selected ?? '')
  if (key === 'ArrowDown') {
    return ids[currentIndex < 0 ? 0 : Math.min(currentIndex + 1, ids.length - 1)]
  }
  return ids[Math.max(currentIndex < 0 ? 0 : currentIndex - 1, 0)]
}

export function SessionWorkspaceSurface({
  bootstrap,
  bootstrapState,
  onTimelineIds,
  onTimelineActions,
}: {
  bootstrap: WebContractBootstrap | undefined
  bootstrapState: 'checking' | 'failed' | 'ready'
  onTimelineIds: (ids: readonly string[]) => void
  onTimelineActions: (
    actions: Pick<CommandContext, 'selectTimeline' | 'openTimelineWindow'> | null,
  ) => void
}) {
  const dispatch = useAppDispatch()
  const app = useAppSelector(selectApp)
  const [draftId, setDraftId] = useState('')
  const [sessionId, setSessionId] = useState<string | null>(null)
  const [restorePosition, setRestorePosition] = useState<string | undefined>()
  const [manualAnchor, setManualAnchor] = useState<SessionWindowAnchor | null>(null)
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set())
  const sessionCapabilitiesAvailable = hasUsableSessionTimeline(bootstrap)
  const session = useQuery({
    queryKey: ['production', 'session-workspace', sessionId, manualAnchor, restorePosition],
    queryFn: async ({ signal }) => {
      const source = await HttpSessionTimelineSource.connect(window.fetch.bind(window), signal)
      const history = new BoundedSessionHistory(sessionId ?? '', source)
      const descriptor = await history.describe(signal)
      const active = BigInt(descriptor.work.active_turn_count) !== BigInt(0)
      const anchor: SessionWindowAnchor =
        manualAnchor ??
        (!active && restorePosition
          ? { kind: 'around', eventSequence: restorePosition }
          : { kind: 'latest' })
      const timelineWindow = await history.load(
        anchor,
        { maxItems: SESSION_WINDOW_ITEMS, maxBytes: SESSION_WINDOW_BYTES },
        signal,
      )
      return {
        anchor,
        active,
        descriptor,
        restored: !active && restorePosition !== undefined,
        source,
        window: timelineWindow,
      }
    },
    enabled: sessionId !== null && sessionCapabilitiesAvailable,
    gcTime: 0,
    placeholderData: (previous) =>
      previous?.descriptor.session_id === sessionId ? previous : undefined,
  })
  const items = useMemo(
    () => visibleSessionItems(session.data?.window.items ?? [], app.detail),
    [app.detail, session.data?.window.items],
  )
  const timelineIds = useMemo(() => items.map((item) => item.address.event_sequence), [items])
  const selected = app.selectedTimeline

  useEffect(() => onTimelineIds(timelineIds), [onTimelineIds, timelineIds])
  useEffect(() => () => onTimelineIds([]), [onTimelineIds])
  useEffect(() => {
    setExpanded((current) => {
      const visible = new Set([...current].filter((id) => timelineIds.includes(id)))
      return visible.size === current.size ? current : visible
    })
    const next = projectedTimelineSelection(selected, timelineIds)
    if (next === selected) return
    dispatch(actions.timelineSelected(next))
    if (next !== null && sessionId !== null) {
      dispatch(actions.logicalPositionRecorded({ sessionId, position: next }))
      requestAnimationFrame(() => {
        document.querySelector<HTMLButtonElement>(`[data-timeline-id="${next}"]`)?.focus()
      })
    }
  }, [dispatch, selected, sessionId, timelineIds])
  const openTimelineAnchor = useCallback(
    (anchor: SessionWindowAnchor) => {
      const refetchCurrentAnchor = sameSessionWindowAnchor(manualAnchor, anchor)
      setManualAnchor(anchor)
      setExpanded(new Set())
      dispatch(actions.timelineSelected(null))
      if (refetchCurrentAnchor) void session.refetch()
    },
    [dispatch, manualAnchor, session],
  )
  const openTimelineWindow = useCallback(
    (anchor: 'first' | 'latest') => openTimelineAnchor({ kind: anchor }),
    [openTimelineAnchor],
  )
  useEffect(() => {
    onTimelineActions({
      selectTimeline: (eventSequence) => {
        if (sessionId !== null) {
          dispatch(actions.logicalPositionRecorded({ sessionId, position: eventSequence }))
        }
        document.querySelector<HTMLButtonElement>(`[data-timeline-id="${eventSequence}"]`)?.focus()
      },
      openTimelineWindow,
    })
    return () => onTimelineActions(null)
  }, [dispatch, onTimelineActions, openTimelineWindow, sessionId])

  useEffect(() => {
    if (manualAnchor?.kind !== 'first' && manualAnchor?.kind !== 'latest') return
    if (session.data?.anchor.kind !== manualAnchor.kind) return
    const boundary = manualAnchor.kind === 'first' ? timelineIds[0] : timelineIds.at(-1)
    if (!boundary) return
    dispatch(actions.timelineSelected(boundary))
    if (sessionId !== null) {
      dispatch(actions.logicalPositionRecorded({ sessionId, position: boundary }))
    }
  }, [dispatch, manualAnchor, session.data?.anchor.kind, sessionId, timelineIds])

  useEffect(() => {
    const restored = restoredTimelineSelection(
      restorePosition,
      session.data?.restored ?? false,
      timelineIds,
    )
    if (restored) dispatch(actions.timelineSelected(restored))
  }, [dispatch, restorePosition, session.data?.restored, timelineIds])

  const openSession = (event: FormEvent) => {
    event.preventDefault()
    const candidate = draftId.trim().toLowerCase()
    if (!isCanonicalSessionId(candidate)) return
    const nextRestorePosition = app.lastLogicalPositions[candidate]
    const refetchCurrentSession =
      candidate === sessionId && manualAnchor === null && restorePosition === nextRestorePosition
    setManualAnchor(null)
    setExpanded(new Set())
    dispatch(actions.timelineSelected(null))
    setRestorePosition(nextRestorePosition)
    setSessionId(candidate)
    if (refetchCurrentSession) void session.refetch()
  }
  const select = (eventSequence: string) => {
    dispatch(actions.timelineSelected(eventSequence))
    if (sessionId !== null) {
      dispatch(actions.logicalPositionRecorded({ sessionId, position: eventSequence }))
    }
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
            onChange={(event) => setDraftId(event.target.value.trim().toLowerCase())}
            pattern={SESSION_ID_HTML_PATTERN}
            required
          />
        </label>
        <button
          type="submit"
          disabled={!isCanonicalSessionId(draftId.trim()) || !sessionCapabilitiesAvailable}
        >
          Open workspace
        </button>
      </form>

      {bootstrapState !== 'ready' ? (
        <section className="surface-empty session-entry" aria-labelledby="session-entry-heading">
          <Radio aria-hidden="true" />
          <div>
            <span className="availability-tag">
              {bootstrapState === 'failed' ? 'Unavailable' : 'Checking'}
            </span>
            <h2 id="session-entry-heading">
              {bootstrapState === 'failed'
                ? 'Session timeline readiness could not be established'
                : 'Checking session timeline capabilities'}
            </h2>
            <p>
              A decoded bootstrap must positively advertise bounded timeline and typed-detail reads.
            </p>
          </div>
        </section>
      ) : !sessionCapabilitiesAvailable && bootstrap ? (
        <section className="surface-empty session-entry" aria-labelledby="session-entry-heading">
          <Radio aria-hidden="true" />
          <div>
            <span className="availability-tag">Unavailable</span>
            <h2 id="session-entry-heading">Session timeline reads are unavailable</h2>
            <p>
              The connected daemon does not advertise both bounded timeline and typed-detail reads.
            </p>
          </div>
        </section>
      ) : sessionId === null ? (
        <section className="surface-empty session-entry" aria-labelledby="session-entry-heading">
          <Radio aria-hidden="true" />
          <div>
            <span className="availability-tag ready">Timeline reads available</span>
            <h2 id="session-entry-heading">Open a known session by immutable identity</h2>
            <p>
              This branch provides bounded descriptor, timeline, and typed detail reads, but no
              session catalog, creation operation, or live follow channel. Enter an exact
              server-issued ID.
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
                {session.data.active
                  ? 'Active · opened near latest'
                  : session.data.restored
                    ? 'Inactive · restored logical position'
                    : 'Inactive · opened near latest'}
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
          <div
            className="session-window-controls"
            role="toolbar"
            aria-label="Timeline window"
            aria-busy={session.isFetching}
          >
            <button type="button" onClick={() => openTimelineWindow('first')}>
              <SkipBack aria-hidden="true" /> First <kbd>gg</kbd>
            </button>
            <button type="button" onClick={() => openTimelineWindow('latest')}>
              <SkipForward aria-hidden="true" /> Latest <kbd>G</kbd>
            </button>
            <button
              type="button"
              disabled={!session.data.window.continuation_before}
              onClick={() => {
                const boundary = session.data.window.continuation_before
                if (boundary) {
                  openTimelineAnchor({ kind: 'before', eventSequence: boundary.event_sequence })
                }
              }}
            >
              Previous window
            </button>
            <button
              type="button"
              disabled={!session.data.window.continuation_after}
              onClick={() => {
                const boundary = session.data.window.continuation_after
                if (boundary) {
                  openTimelineAnchor({ kind: 'after', eventSequence: boundary.event_sequence })
                }
              }}
            >
              Next window
            </button>
            <span>
              {session.data.window.items.length} bounded items ·{' '}
              {session.data.window.projected_structured_bytes} B
            </span>
          </div>
          <ol
            className="session-timeline"
            aria-label="Session timeline"
            onKeyDown={(event) => {
              if (event.key === 'Home' || event.key === 'End') {
                event.preventDefault()
                openTimelineWindow(event.key === 'Home' ? 'first' : 'latest')
                return
              }
              const target = timelineArrowTarget(timelineIds, selected, event.key)
              if (!target) return
              event.preventDefault()
              select(target)
              event.currentTarget
                .querySelector<HTMLButtonElement>(`[data-timeline-id="${target}"]`)
                ?.focus()
            }}
          >
            {items.map((item) => {
              const id = item.address.event_sequence
              const isExpanded = expanded.has(id)
              return (
                <li key={id} className={selected === id ? 'selected' : undefined}>
                  <button
                    type="button"
                    className="session-item-summary"
                    data-timeline-id={id}
                    aria-expanded={isExpanded}
                    aria-current={selected === id ? 'true' : undefined}
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
