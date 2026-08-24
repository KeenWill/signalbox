import { useQuery } from '@tanstack/react-query'
import { ChevronDown, ChevronLeft, ChevronRight, Radio, SkipBack, SkipForward } from 'lucide-react'
import { type FormEvent, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { MissingAttachmentState } from './features/artifacts/ArtifactAttachments'
import type { WebSessionTimelineWindow } from './generated/web-contract.mjs'
import {
  BoundedSessionHistory,
  HttpSessionTimelineSource,
  type SessionWindowAnchor,
} from './session-timeline/model'
import { actions, selectApp, useAppDispatch, useAppSelector } from './state'

const SESSION_ID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i
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

export const reconcileTimelineSelection = (
  selected: string | null,
  visibleIds: readonly string[],
): string | null =>
  selected !== null && !visibleIds.includes(selected) ? (visibleIds[0] ?? null) : selected

export const retainSameSessionPlaceholder = <T extends { sessionId: string }>(
  previousData: T | undefined,
  sessionId: string | null,
): T | undefined => (previousData?.sessionId === sessionId ? previousData : undefined)

export function SessionWorkspaceSurface({
  onFirstWindowAction,
  onLatestWindowAction,
  onSessionId,
  onTimelineIds,
}: {
  onFirstWindowAction: (action: (() => void) | null) => void
  onLatestWindowAction: (action: (() => void) | null) => void
  onSessionId: (sessionId: string | null) => void
  onTimelineIds: (ids: readonly string[]) => void
}) {
  const dispatch = useAppDispatch()
  const app = useAppSelector(selectApp)
  const [draftId, setDraftId] = useState('')
  const [sessionId, setSessionId] = useState<string | null>(null)
  const [manualAnchor, setManualAnchor] = useState<SessionWindowAnchor | null>(null)
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set())
  const timelineRef = useRef<HTMLOListElement>(null)
  const remembered = sessionId === null ? undefined : app.lastLogicalPositions[sessionId]
  const session = useQuery({
    queryKey: ['production', 'session-workspace', sessionId, manualAnchor],
    queryFn: async ({ signal }) => {
      const source = await HttpSessionTimelineSource.connect(window.fetch.bind(window), signal)
      const history = new BoundedSessionHistory(sessionId ?? '', source)
      const descriptor = await history.describe(signal)
      const active = BigInt(descriptor.work.active_turn_count) !== BigInt(0)
      const anchor: SessionWindowAnchor =
        manualAnchor ??
        (!active && remembered ? { kind: 'around', eventSequence: remembered } : { kind: 'latest' })
      const timelineWindow = await history.load(
        anchor,
        { maxItems: SESSION_WINDOW_ITEMS, maxBytes: SESSION_WINDOW_BYTES },
        signal,
      )
      return {
        active,
        anchor,
        descriptor,
        restoredRememberedPosition: manualAnchor === null && !active && remembered !== undefined,
        sessionId: sessionId ?? '',
        window: timelineWindow,
      }
    },
    enabled: sessionId !== null,
    gcTime: 0,
    placeholderData: (previousData) => retainSameSessionPlaceholder(previousData, sessionId),
  })
  const items = useMemo(
    () => visibleSessionItems(session.data?.window.items ?? [], app.detail),
    [app.detail, session.data?.window.items],
  )
  const timelineIds = useMemo(() => items.map((item) => item.address.event_sequence), [items])
  const selected = app.selectedTimeline

  useEffect(() => onTimelineIds(timelineIds), [onTimelineIds, timelineIds])
  useEffect(() => {
    const visible = new Set(timelineIds)
    setExpanded((current) => {
      const retained = new Set([...current].filter((id) => visible.has(id)))
      return retained.size === current.size ? current : retained
    })
  }, [timelineIds])
  useEffect(() => {
    if (!session.data) return
    const reconciled = reconcileTimelineSelection(selected, timelineIds)
    if (reconciled !== selected) dispatch(actions.timelineSelected(reconciled))
  }, [dispatch, selected, session.data, timelineIds])
  useEffect(() => () => onTimelineIds([]), [onTimelineIds])
  useEffect(() => onSessionId(sessionId), [onSessionId, sessionId])
  useEffect(() => () => onSessionId(null), [onSessionId])
  useEffect(() => {
    if (selected === null) return
    const row = timelineRef.current?.querySelector<HTMLElement>(
      `[data-event-sequence="${CSS.escape(selected)}"]`,
    )
    row?.focus({ preventScroll: true })
    row?.scrollIntoView({ block: 'nearest' })
  }, [selected])
  const showFirstWindow = useCallback(() => setManualAnchor({ kind: 'first' }), [])
  const showLatestWindow = useCallback(() => setManualAnchor({ kind: 'latest' }), [])
  // Window commands act on an opened session; registering them earlier would advertise
  // first/latest actions that cannot load or select anything.
  useEffect(() => {
    if (sessionId === null) return
    onFirstWindowAction(showFirstWindow)
    return () => onFirstWindowAction(null)
  }, [onFirstWindowAction, sessionId, showFirstWindow])
  useEffect(() => {
    if (sessionId === null) return
    onLatestWindowAction(showLatestWindow)
    return () => onLatestWindowAction(null)
  }, [onLatestWindowAction, sessionId, showLatestWindow])

  const openSession = (event: FormEvent) => {
    event.preventDefault()
    const candidate = draftId.trim().toLowerCase()
    if (!isCanonicalSessionId(candidate)) return
    setManualAnchor(null)
    setExpanded(new Set())
    dispatch(actions.timelineSelected(null))
    if (candidate === sessionId && manualAnchor === null) void session.refetch()
    setSessionId(candidate)
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
            onChange={(event) => setDraftId(event.target.value)}
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
                {session.data.anchor.kind === 'latest'
                  ? `${session.data.active ? 'Active' : 'Inactive'} · opened near latest`
                  : session.data.restoredRememberedPosition
                    ? 'Inactive · restored logical position'
                    : `${session.data.active ? 'Active' : 'Inactive'} · inspecting ${session.data.anchor.kind} window`}
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
            <button type="button" onClick={showFirstWindow}>
              <SkipBack aria-hidden="true" /> First <kbd>gg</kbd>
            </button>
            <button
              type="button"
              disabled={session.data.window.continuation_before === null}
              onClick={() => {
                const continuation = session.data.window.continuation_before
                if (continuation) {
                  setManualAnchor({ kind: 'before', eventSequence: continuation.event_sequence })
                }
              }}
            >
              <ChevronLeft aria-hidden="true" /> Previous window
            </button>
            <button
              type="button"
              disabled={session.data.window.continuation_after === null}
              onClick={() => {
                const continuation = session.data.window.continuation_after
                if (continuation) {
                  setManualAnchor({ kind: 'after', eventSequence: continuation.event_sequence })
                }
              }}
            >
              Next window <ChevronRight aria-hidden="true" />
            </button>
            <button type="button" onClick={showLatestWindow}>
              <SkipForward aria-hidden="true" /> Latest <kbd>G</kbd>
            </button>
            <span>
              {session.data.window.items.length} bounded items ·{' '}
              {session.data.window.projected_structured_bytes} B
            </span>
          </div>
          <ol ref={timelineRef} className="session-timeline" aria-label="Session timeline">
            {items.map((item) => {
              const id = item.address.event_sequence
              const isExpanded = expanded.has(id)
              return (
                <li key={id} className={selected === id ? 'selected' : undefined}>
                  <button
                    type="button"
                    className="session-item-summary"
                    data-event-sequence={id}
                    aria-current={selected === id ? 'true' : undefined}
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
                    <div className="session-item-expanded">
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
                      {item.kind === 'input_accepted' && (
                        <MissingAttachmentState placement="transcript" />
                      )}
                    </div>
                  )}
                </li>
              )
            })}
          </ol>
          <section className="session-composer" aria-labelledby="session-composer-heading">
            <header>
              <div>
                <span className="eyebrow">Input surface</span>
                <h3 id="session-composer-heading">Composer</h3>
              </div>
              <span className="availability-tag">Committed · unavailable</span>
            </header>
            <MissingAttachmentState placement="composer" />
          </section>
        </section>
      )}
    </div>
  )
}
