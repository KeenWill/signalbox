import { type QueryClient, useQuery, useQueryClient } from '@tanstack/react-query'
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
import { MissingAttachmentState } from './features/artifacts/ArtifactAttachments'
import type { WebSessionTimelineWindow } from './generated/web-contract.mjs'
import type { SessionTranscriptLimits } from './product'
import { SessionComposer } from './SessionComposer'
import { SessionTranscriptText } from './SessionTranscriptText'
import {
  BoundedSessionHistory,
  HttpSessionTimelineSource,
  type SessionWindowAnchor,
} from './session-timeline/model'
import { SESSION_WINDOW_BYTES, SESSION_WINDOW_ITEMS } from './session-workspace'
import {
  actions,
  selectApp,
  selectSessionSync,
  store,
  useAppDispatch,
  useAppSelector,
} from './state'

const SESSION_ID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i
const NATIVE_SESSION_ID_PATTERN = String.raw`\s*[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\s*`
const MAX_CACHED_SESSION_WORKSPACES = 4
type TimelineCapability = 'checking' | 'available' | 'unavailable'

export const isCanonicalSessionId = (value: string): boolean => SESSION_ID_PATTERN.test(value)
export const sessionWorkspaceQueryKey = (sessionId: string | null) =>
  ['production', 'session-workspace', sessionId] as const

export const evictInactiveSessionWorkspaceQueries = (
  queryClient: QueryClient,
  retainedSessionId: string | null,
): void => {
  const inactiveQueries = queryClient
    .getQueryCache()
    .findAll({ queryKey: ['production', 'session-workspace'] })
    .filter(
      (query) =>
        typeof query.queryKey[2] === 'string' &&
        query.queryKey[2] !== retainedSessionId &&
        query.getObserversCount() === 0,
    )
    .sort((left, right) => right.state.dataUpdatedAt - left.state.dataUpdatedAt)

  for (const query of inactiveQueries.slice(MAX_CACHED_SESSION_WORKSPACES - 1)) {
    queryClient.removeQueries({ queryKey: query.queryKey, exact: true })
  }
}

export const sessionHasLiveWork = (activeTurnCount: string, queuedTurnCount: string): boolean =>
  BigInt(activeTurnCount) !== BigInt(0) || BigInt(queuedTurnCount) !== BigInt(0)

export const reconcileVisibleSessionSelection = (
  selected: string | null,
  visibleIds: readonly string[],
  preferred: string | null = null,
): string | null =>
  selected !== null && visibleIds.includes(selected)
    ? selected
    : preferred !== null && visibleIds.includes(preferred)
      ? preferred
      : (visibleIds[0] ?? null)

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
          'goal_turn_retired',
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
  initialSessionId,
  focusEntry,
  onSessionOpen,
  onTimelineIds,
  onTimelineWindowAvailable,
  onWindowRequestConsumed,
  timelineCapability,
  transcriptAvailable,
  transcriptLimits,
  timelineRef,
  windowRequest,
}: {
  initialSessionId?: string
  focusEntry: boolean
  onSessionOpen: (sessionId: string) => void
  onTimelineIds: (ids: readonly string[]) => void
  onTimelineWindowAvailable: (available: boolean) => void
  onWindowRequestConsumed: () => void
  timelineCapability: TimelineCapability
  transcriptAvailable: boolean
  transcriptLimits: SessionTranscriptLimits
  timelineRef: RefObject<HTMLDivElement | null>
  windowRequest: { anchor: 'first' | 'latest'; attempt: number } | null
}) {
  const dispatch = useAppDispatch()
  const queryClient = useQueryClient()
  const app = useAppSelector(selectApp)
  const entryInput = useRef<HTMLInputElement>(null)
  useEffect(() => {
    if (focusEntry) entryInput.current?.focus()
  }, [focusEntry])
  const [draftId, setDraftId] = useState(initialSessionId ?? '')
  const [sessionId, setSessionId] = useState<string | null>(initialSessionId ?? null)
  const [awaitingSessionId, setAwaitingSessionId] = useState<string | null>(null)
  const [openingPosition, setOpeningPosition] = useState<string | undefined>(
    initialSessionId === undefined ? undefined : app.lastLogicalPositions[initialSessionId],
  )
  const [refetchRequest, setRefetchRequest] = useState(0)
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set())
  const rowRefs = useRef(new Map<string, HTMLDivElement>())
  const manualAnchorRef = useRef<SessionWindowAnchor | null>(null)
  const handledRefetchRequest = useRef(0)
  const boundaryRequest = useRef(0)
  const session = useQuery({
    queryKey: sessionWorkspaceQueryKey(sessionId),
    queryFn: async ({ signal }) => {
      const requestedSessionId = sessionId ?? ''
      const cached = queryClient.getQueryData<{ history: BoundedSessionHistory }>(
        sessionWorkspaceQueryKey(requestedSessionId),
      )
      const history =
        cached?.history ??
        new BoundedSessionHistory(
          requestedSessionId,
          await HttpSessionTimelineSource.connect(window.fetch.bind(window), signal),
        )
      const descriptor = await history.describe(signal)
      const active = sessionHasLiveWork(
        descriptor.work.active_turn_count,
        descriptor.work.queued_turn_count,
      )
      let anchor: SessionWindowAnchor =
        manualAnchorRef.current ??
        (!active && openingPosition
          ? { kind: 'around', eventSequence: openingPosition }
          : { kind: 'latest' })
      let timelineWindow = await history.load(
        anchor,
        { maxItems: SESSION_WINDOW_ITEMS, maxBytes: SESSION_WINDOW_BYTES },
        signal,
      )
      let reconciledDescriptor = await history.describe(signal)
      let reconciledActive = sessionHasLiveWork(
        reconciledDescriptor.work.active_turn_count,
        reconciledDescriptor.work.queued_turn_count,
      )
      if (manualAnchorRef.current === null && anchor.kind === 'around' && reconciledActive) {
        anchor = { kind: 'latest' }
        timelineWindow = await history.load(
          anchor,
          { maxItems: SESSION_WINDOW_ITEMS, maxBytes: SESSION_WINDOW_BYTES },
          signal,
        )
        reconciledDescriptor = await history.describe(signal)
        reconciledActive = sessionHasLiveWork(
          reconciledDescriptor.work.active_turn_count,
          reconciledDescriptor.work.queued_turn_count,
        )
      }
      const latestWindowAddress = timelineWindow.items.at(-1)?.address.event_sequence
      if (
        latestWindowAddress !== undefined &&
        BigInt(latestWindowAddress) > BigInt(reconciledDescriptor.latest_address.event_sequence)
      ) {
        throw new TypeError('timeline window exceeds the reconciled descriptor')
      }
      return {
        active: reconciledActive,
        anchor,
        descriptor: reconciledDescriptor,
        history,
        window: timelineWindow,
      }
    },
    enabled: sessionId !== null && timelineCapability === 'available',
  })
  const refetchSession = session.refetch
  const synchronization = useAppSelector(selectSessionSync)
  const live = synchronization.sessionId === sessionId ? synchronization.snapshot : null
  const followFailed = synchronization.sessionId === sessionId && synchronization.phase === 'failed'
  useEffect(() => {
    dispatch(actions.sessionFollowRequested(timelineCapability === 'available' ? sessionId : null))
    return () => {
      dispatch(actions.sessionFollowRequested(null))
    }
  }, [dispatch, sessionId, timelineCapability])
  const displayedSession =
    session.isSuccess && awaitingSessionId !== sessionId ? session.data : undefined
  const items = useMemo(
    () => visibleSessionItems(displayedSession?.window.items ?? [], app.detail),
    [app.detail, displayedSession?.window.items],
  )
  const timelineIds = useMemo(() => items.map((item) => item.address.event_sequence), [items])
  const loadWindow = useCallback(
    async (anchor: 'first' | 'latest', control?: HTMLButtonElement) => {
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
      if (control === undefined) timelineRef.current?.focus()
    },
    [dispatch, refetchSession, timelineRef],
  )
  const toggleSelectedExpansion = useCallback(() => {
    const eventSequence = store.getState().app.selectedTimeline
    if (eventSequence === null || !timelineIds.includes(eventSequence)) return
    setExpanded((current) => {
      const next = new Set(current)
      if (next.has(eventSequence)) next.delete(eventSequence)
      else next.add(eventSequence)
      return next
    })
  }, [timelineIds])

  useEffect(() => onTimelineIds(timelineIds), [onTimelineIds, timelineIds])
  useEffect(() => () => onTimelineIds([]), [onTimelineIds])
  useEffect(() => {
    const preferred =
      displayedSession?.anchor.kind === 'around' ? displayedSession.anchor.eventSequence : null
    const reconciled = reconcileVisibleSessionSelection(
      app.selectedTimeline,
      timelineIds,
      preferred,
    )
    if (reconciled !== app.selectedTimeline) {
      dispatch(actions.timelineSelected(reconciled))
    }
  }, [app.selectedTimeline, dispatch, displayedSession?.anchor, timelineIds])
  useEffect(() => {
    setExpanded((current) =>
      pruneExpandedSessionItems(current, displayedSession?.window.items ?? []),
    )
  }, [displayedSession?.window.items])
  useEffect(
    () =>
      onTimelineWindowAvailable(
        timelineCapability === 'available' && displayedSession !== undefined,
      ),
    [displayedSession, onTimelineWindowAvailable, timelineCapability],
  )
  useEffect(() => () => onTimelineWindowAvailable(false), [onTimelineWindowAvailable])
  useEffect(() => {
    evictInactiveSessionWorkspaceQueries(queryClient, sessionId)
  }, [queryClient, sessionId])
  useEffect(() => {
    if (windowRequest === null || sessionId === null) return
    void loadWindow(windowRequest.anchor)
    onWindowRequestConsumed()
  }, [loadWindow, onWindowRequestConsumed, sessionId, windowRequest])
  useEffect(() => {
    if (
      awaitingSessionId === sessionId &&
      !session.isFetching &&
      (session.isSuccess || session.isError)
    ) {
      setAwaitingSessionId(null)
    }
  }, [awaitingSessionId, session.isError, session.isFetching, session.isSuccess, sessionId])
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
    if (
      sessionId !== null &&
      app.selectedTimeline !== null &&
      displayedSession?.window.items.some(
        (item) => item.address.event_sequence === app.selectedTimeline,
      )
    ) {
      dispatch(
        actions.logicalPositionRecorded({
          sessionId,
          position: app.selectedTimeline,
        }),
      )
    }
  }, [app.selectedTimeline, dispatch, displayedSession?.window.items, sessionId])
  useEffect(() => {
    if (app.selectedTimeline !== null) {
      rowRefs.current.get(app.selectedTimeline)?.scrollIntoView({ block: 'nearest' })
    }
  }, [app.selectedTimeline])

  const openSession = (candidate: string) => {
    onSessionOpen(candidate)
    const reopeningCurrentSession = candidate === sessionId
    setOpeningPosition(app.lastLogicalPositions[candidate])
    manualAnchorRef.current = null
    setExpanded(new Set())
    if (!reopeningCurrentSession) {
      setAwaitingSessionId(candidate)
      dispatch(actions.timelineSelected(null))
    }
    setSessionId(candidate)
    if (reopeningCurrentSession) {
      boundaryRequest.current += 1
      setRefetchRequest((current) => current + 1)
    }
  }
  const submitSession = (event: FormEvent) => {
    event.preventDefault()
    const candidate = draftId.trim().toLowerCase()
    invokeCommand('session.open', {
      dispatch,
      getState: store.getState,
      timelineIds,
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => timelineRef.current?.focus(),
      sessionId:
        isCanonicalSessionId(candidate) && timelineCapability === 'available'
          ? candidate
          : undefined,
      openSession,
    })
  }
  const selected = app.selectedTimeline
  const select = (eventSequence: string) => {
    dispatch(actions.timelineSelected(eventSequence))
  }
  const invokeTimelineCommand = (
    command:
      | 'selection.next'
      | 'selection.previous'
      | 'selection.first'
      | 'selection.last'
      | 'selection.toggleExpansion',
    control?: HTMLButtonElement,
  ) =>
    invokeCommand(command, {
      dispatch,
      getState: store.getState,
      timelineIds,
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      timelineWindowAvailable: displayedSession !== undefined,
      focusTimeline: () => timelineRef.current?.focus(),
      loadTimelineWindow: (anchor) => loadWindow(anchor, control),
      toggleTimelineExpansion: toggleSelectedExpansion,
    })
  const handleTimelineKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if ((event.key === 'Enter' || event.key === ' ') && selected !== null) {
      event.preventDefault()
      invokeTimelineCommand('selection.toggleExpansion')
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
    invokeTimelineCommand(command)
  }
  const invokeBoundaryCommand = (
    command: 'selection.first' | 'selection.last',
    control: HTMLButtonElement,
  ) => {
    control.focus()
    invokeTimelineCommand(command, control)
  }

  return (
    <div className="surface-body session-workspace-surface">
      <form className="session-open-form" onSubmit={submitSession}>
        <label>
          Exact session ID
          <input
            ref={entryInput}
            aria-label="Exact session ID"
            placeholder="00000000-0000-0000-0000-000000000000"
            value={draftId}
            onChange={(event) => setDraftId(event.target.value.trim())}
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
                ? 'Enter an exact server-issued ID to read bounded history, follow live updates, and send a message.'
                : 'The validated daemon bootstrap has not authorized bounded session timeline reads. Signalbox will not call or advertise that surface until the capability is available.'}
            </p>
          </div>
        </section>
      ) : session.isError ? (
        <p className="session-load-state" role="alert">
          The daemon could not provide this bounded session window: {session.error.message}
        </p>
      ) : displayedSession === undefined ? (
        <p className="session-load-state" role="status">
          Loading descriptor and bounded history…
        </p>
      ) : (
        <section className="session-workspace" aria-labelledby="session-workspace-heading">
          <p className="sr-only" role="status">
            Session workspace loaded for {sessionId}.
          </p>
          <header className="session-workspace-header">
            <div>
              <span className="eyebrow">Stable timeline identity</span>
              <h2 id="session-workspace-heading">{sessionId}</h2>
              <p>
                {displayedSession.active ? 'Active' : 'Inactive'} ·{' '}
                {displayedSession.anchor.kind === 'first'
                  ? 'opened at first'
                  : displayedSession.anchor.kind === 'latest'
                    ? 'opened near latest'
                    : 'restored logical position'}
              </p>
            </div>
            <dl className="session-telemetry">
              <div>
                <dt>Items</dt>
                <dd>{displayedSession.descriptor.sizes.item_count}</dd>
              </div>
              <div>
                <dt>Active</dt>
                <dd>{displayedSession.descriptor.work.active_turn_count}</dd>
              </div>
              <div>
                <dt>Queued</dt>
                <dd>{displayedSession.descriptor.work.queued_turn_count}</dd>
              </div>
              <div>
                <dt>Observed</dt>
                <dd>{displayedSession.descriptor.observed_through}</dd>
              </div>
            </dl>
          </header>
          <div className="session-window-controls" role="toolbar" aria-label="Timeline window">
            <button
              type="button"
              onClick={(event) => invokeBoundaryCommand('selection.first', event.currentTarget)}
            >
              <SkipBack aria-hidden="true" /> First <kbd>gg</kbd>
            </button>
            <button
              type="button"
              onClick={(event) => invokeBoundaryCommand('selection.last', event.currentTarget)}
            >
              <SkipForward aria-hidden="true" /> Latest <kbd>G</kbd>
            </button>
            <button
              type="button"
              disabled={!displayedSession.window.continuation_before}
              onClick={() => {
                const address = displayedSession.window.continuation_before?.event_sequence
                if (address) {
                  manualAnchorRef.current = { kind: 'before', eventSequence: address }
                  void refetchSession()
                }
              }}
            >
              Previous window
            </button>
            <button
              type="button"
              disabled={!displayedSession.window.continuation_after}
              onClick={() => {
                const address = displayedSession.window.continuation_after?.event_sequence
                if (address) {
                  manualAnchorRef.current = { kind: 'after', eventSequence: address }
                  void refetchSession()
                }
              }}
            >
              Next window
            </button>
            <span>
              {displayedSession.window.items.length} bounded items ·{' '}
              {displayedSession.window.projected_structured_bytes} B
            </span>
          </div>
          <p className="session-live-status" role="status">
            {followFailed ? (
              <>
                Live updates unavailable.{' '}
                <button
                  type="button"
                  onClick={() => dispatch(actions.sessionFollowReconnectRequested())}
                >
                  Reconnect live updates
                </button>
              </>
            ) : synchronization.sessionId === sessionId && synchronization.phase === 'resyncing' ? (
              'Resynchronizing live session…'
            ) : live ? (
              'Following live session'
            ) : (
              'Connecting live session…'
            )}
          </p>
          {(live?.reconciliation || live?.runner) && (
            <div className="session-live-facts">
              {live.reconciliation && (
                <span className="availability-tag">
                  Awaiting reconciliation · {live.reconciliation.kind.replaceAll('_', ' ')}
                </span>
              )}
              {live.runner && (
                <span className="availability-tag">
                  Runner · {live.runner.state.replaceAll('_', ' ')}
                  {live.runner.state === 'pinned' && ` · ${live.runner.connection_health}`}
                </span>
              )}
            </div>
          )}
          {synchronization.sessionId === sessionId && synchronization.drafts.length > 0 && (
            <section className="provider-drafts" aria-label="Provider draft">
              <span>Streaming draft · discarded on resync</span>
              {synchronization.drafts.map((draft) => (
                <p key={draft.key}>{draft.content}</p>
              ))}
            </section>
          )}
          {transcriptAvailable &&
            displayedSession.descriptor.sizes.projected_text_bytes !== '0' && (
              <SessionTranscriptText
                sessionId={sessionId ?? ''}
                first={
                  displayedSession.window.items[0]?.address.event_sequence ??
                  displayedSession.descriptor.first_address.event_sequence
                }
                through={
                  displayedSession.window.items.at(-1)?.address.event_sequence ??
                  displayedSession.descriptor.latest_address.event_sequence
                }
                observed={displayedSession.descriptor.observed_through}
                limits={transcriptLimits}
              />
            )}
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
                    invokeTimelineCommand('selection.toggleExpansion')
                    timelineRef.current?.focus()
                  }}
                  onKeyDown={(event) => {
                    if (event.key !== 'Enter' && event.key !== ' ') return
                    event.preventDefault()
                    event.stopPropagation()
                    select(id)
                    invokeTimelineCommand('selection.toggleExpansion')
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
                    <>
                      <dl id={`session-timeline-detail-${id}`} className="session-item-detail">
                        <div>
                          <dt>Address</dt>
                          <dd>
                            {sessionId}:{id}
                          </dd>
                        </div>
                        <div>
                          <dt>Projection</dt>
                          <dd>
                            {transcriptAvailable
                              ? 'Durable event metadata; message text appears in the transcript above'
                              : 'Durable event metadata; transcript text is unavailable'}
                          </dd>
                        </div>
                      </dl>
                      {item.kind === 'input_accepted' && (
                        <MissingAttachmentState placement="transcript" />
                      )}
                    </>
                  )}
                </div>
              )
            })}
          </div>
        </section>
      )}
      {sessionId !== null && timelineCapability === 'available' && (
        <SessionComposer
          key={sessionId}
          sessionId={sessionId ?? ''}
          activeState={live ? (live.active?.state.kind ?? null) : undefined}
          stateUnavailable={followFailed && live === null}
          onAccepted={refetchSession}
        />
      )}
    </div>
  )
}
