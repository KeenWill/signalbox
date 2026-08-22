import { useQuery } from '@tanstack/react-query'
import { useVirtualizer } from '@tanstack/react-virtual'
import {
  ChevronDown,
  ChevronRight,
  ListFilter,
  Radio,
  Search,
  SkipBack,
  SkipForward,
} from 'lucide-react'
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
import { type CommandContext, invokeCommand } from './commands'
import type {
  WebAttentionSummary,
  WebSessionLiveActiveState,
  WebSessionTimelineWindow,
} from './generated/web-contract.mjs'
import {
  appendCatalog,
  applyAttentionEvent,
  applyLiveEvent,
  beginLiveResync,
  type CatalogPresentation,
  type CatalogSort,
  EMPTY_CATALOG_PRESENTATION,
  EMPTY_LIVE_PRESENTATION,
  HttpSessionProjectionSource,
  MAX_CATALOG_ROWS,
  replaceCatalog,
  SessionProjectionSynchronizer,
} from './session-live/model'
import {
  BoundedSessionHistory,
  HttpSessionTimelineSource,
  type SessionWindowAnchor,
} from './session-timeline/model'
import { actions, selectApp, store, useAppDispatch, useAppSelector } from './state'

const SESSION_WINDOW_ITEMS = 80
const SESSION_WINDOW_BYTES = 64 * 1024
type TimelineCapability = 'checking' | 'available' | 'unavailable'

export const sessionWorkspaceQueryKey = (sessionId: string | null) =>
  ['production', 'session-workspace', sessionId] as const

export interface SessionCommandControls {
  catalogAvailable: boolean
  workspaceAvailable: boolean
  focusSearch: () => void
  applySearch: () => void
  loadMore: () => void
  toggleSort: () => void
  select: (offset: -1 | 1) => void
  switchSession: (offset: -1 | 1) => void
  openSelected: () => void
}

const activityLabel = (unixMilliseconds: string) => {
  const milliseconds = Number(unixMilliseconds)
  return Number.isSafeInteger(milliseconds)
    ? new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(
        new Date(milliseconds),
      )
    : unixMilliseconds
}

const activityDateTime = (unixMilliseconds: string) => {
  const milliseconds = Number(unixMilliseconds)
  return Number.isSafeInteger(milliseconds) ? new Date(milliseconds).toISOString() : undefined
}

const stateLabel = (state: WebAttentionSummary['state']) => state.replaceAll('_', ' ')

const activeStateLabel = (state: WebSessionLiveActiveState) => {
  switch (state.kind) {
    case 'running':
      return 'Running'
    case 'awaiting_model_call_recovery':
      return 'Awaiting model recovery'
    case 'awaiting_tool_approval':
      return 'Awaiting approval'
    case 'awaiting_child':
      return 'Awaiting child session'
    case 'awaiting_tool_recovery':
      return 'Awaiting tool recovery'
    case 'awaiting_runner_recovery':
      return 'Awaiting runner recovery'
  }
}

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
  maxNdjsonRecordBytes,
  onCommandControls,
  onTimelineIds,
  onTimelineWindowAvailable,
  onWindowRequestConsumed,
  timelineCapability,
  timelineRef,
  windowRequest,
}: {
  maxNdjsonRecordBytes: number
  onCommandControls: (controls: SessionCommandControls | null) => void
  onTimelineIds: (ids: readonly string[]) => void
  onTimelineWindowAvailable: (available: boolean) => void
  onWindowRequestConsumed: () => void
  timelineCapability: TimelineCapability
  timelineRef: RefObject<HTMLDivElement | null>
  windowRequest: { anchor: 'first' | 'latest'; attempt: number } | null
}) {
  const dispatch = useAppDispatch()
  const app = useAppSelector(selectApp)
  const source = useMemo(
    () => new HttpSessionProjectionSource(window.fetch.bind(window), maxNdjsonRecordBytes),
    [maxNdjsonRecordBytes],
  )
  const synchronizer = useMemo(() => new SessionProjectionSynchronizer(source), [source])
  const searchRef = useRef<HTMLInputElement>(null)
  const catalogRef = useRef<HTMLDivElement>(null)
  const [searchDraft, setSearchDraft] = useState('')
  const [search, setSearch] = useState('')
  const [sort, setSort] = useState<CatalogSort>('last_activity_desc')
  const [catalogPresentation, setCatalogPresentation] = useState<CatalogPresentation>(
    EMPTY_CATALOG_PRESENTATION,
  )
  const [catalogFollowState, setCatalogFollowState] = useState<'connecting' | 'live' | 'retrying'>(
    'connecting',
  )
  const [loadingMore, setLoadingMore] = useState(false)
  const [catalogPageError, setCatalogPageError] = useState(false)
  const [selectedCatalogId, setSelectedCatalogId] = useState<string | null>(null)
  const [sessionId, setSessionId] = useState<string | null>(null)
  const [openingPosition, setOpeningPosition] = useState<string | undefined>()
  const [refetchRequest, setRefetchRequest] = useState(0)
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set())
  const [livePresentation, setLivePresentation] = useState(EMPTY_LIVE_PRESENTATION)
  const [liveConnection, setLiveConnection] = useState<'idle' | 'connecting' | 'live' | 'retrying'>(
    'idle',
  )
  const rowRefs = useRef(new Map<string, HTMLDivElement>())
  const manualAnchorRef = useRef<SessionWindowAnchor | null>(null)
  const handledRefetchRequest = useRef(0)
  const boundaryRequest = useRef(0)
  const catalog = useQuery({
    queryKey: ['production', 'session-catalog', search, sort],
    queryFn: ({ signal }) => source.catalogPage({ search, sort }, undefined, signal),
    enabled: timelineCapability === 'available',
  })
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
  const catalogRows = catalogPresentation.summaries
  const catalogVirtualizer = useVirtualizer({
    count: catalogRows.length,
    getScrollElement: () => catalogRef.current,
    estimateSize: () => 70,
    overscan: 8,
  })
  const combinedItems = useMemo(() => {
    const historical = session.data?.window.items ?? []
    const existing = new Set(historical.map((item) => item.address.event_sequence))
    const durable = livePresentation.durable
      .filter((item) => !existing.has(item.address.event_sequence))
      .map((item) => ({
        address: item.address,
        kind: item.event_kind,
        projected_structured_bytes: 0,
      }))
    return [...historical, ...durable].sort((left, right) =>
      BigInt(left.address.event_sequence) < BigInt(right.address.event_sequence) ? -1 : 1,
    )
  }, [livePresentation.durable, session.data?.window.items])
  const refetchSession = session.refetch
  const items = useMemo(
    () => visibleSessionItems(combinedItems, app.detail),
    [app.detail, combinedItems],
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

  useEffect(() => {
    if (!catalog.data) return
    setCatalogPresentation(replaceCatalog(catalog.data))
    setSelectedCatalogId((current) =>
      catalog.data.summaries.some((summary) => summary.session_id === current)
        ? current
        : (catalog.data.summaries[0]?.session_id ?? null),
    )
  }, [catalog.data])
  useEffect(() => {
    if (timelineCapability !== 'available') return
    return synchronizer.followAttention(
      (event) => setCatalogPresentation((current) => applyAttentionEvent(current, event)),
      setCatalogFollowState,
    )
  }, [synchronizer, timelineCapability])
  useEffect(() => {
    if (sessionId === null || timelineCapability !== 'available') {
      setLiveConnection('idle')
      setLivePresentation(EMPTY_LIVE_PRESENTATION)
      return
    }
    setLivePresentation(EMPTY_LIVE_PRESENTATION)
    return synchronizer.followSession(
      sessionId,
      (event) => {
        setLivePresentation((current) => applyLiveEvent(current, event))
        if (event.kind === 'durable') void session.refetch()
      },
      (state) => {
        setLiveConnection(state)
        if (state === 'retrying') setLivePresentation(beginLiveResync)
      },
    )
  }, [session.refetch, sessionId, synchronizer, timelineCapability])
  useEffect(() => onTimelineIds(timelineIds), [onTimelineIds, timelineIds])
  useEffect(() => () => onTimelineIds([]), [onTimelineIds])
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

  const openSessionById = useCallback(
    (candidate: string) => {
      if (timelineCapability !== 'available') return
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
    },
    [app.lastLogicalPositions, dispatch, sessionId, timelineCapability],
  )
  const selectRelativeSession = useCallback(
    (offset: -1 | 1) => {
      const currentIndex = catalogRows.findIndex(
        (summary) => summary.session_id === selectedCatalogId,
      )
      const nextIndex = Math.max(0, Math.min(currentIndex + offset, catalogRows.length - 1))
      const next = catalogRows[nextIndex]
      if (!next) return
      setSelectedCatalogId(next.session_id)
      catalogVirtualizer.scrollToIndex(nextIndex, { align: 'auto' })
      catalogRef.current?.focus()
    },
    [catalogRows, catalogVirtualizer, selectedCatalogId],
  )
  const openSelectedSession = useCallback(() => {
    if (selectedCatalogId) openSessionById(selectedCatalogId)
  }, [openSessionById, selectedCatalogId])
  const switchRelativeSession = useCallback(
    (offset: -1 | 1) => {
      const currentIndex = catalogRows.findIndex((summary) => summary.session_id === sessionId)
      const nextIndex = Math.max(0, Math.min(currentIndex + offset, catalogRows.length - 1))
      const next = catalogRows[nextIndex]
      if (!next) return
      setSelectedCatalogId(next.session_id)
      openSessionById(next.session_id)
    },
    [catalogRows, openSessionById, sessionId],
  )
  const applySearch = useCallback(() => {
    setCatalogPresentation(EMPTY_CATALOG_PRESENTATION)
    setSearch(searchDraft.trim())
  }, [searchDraft])
  const toggleSort = useCallback(() => {
    setCatalogPresentation(EMPTY_CATALOG_PRESENTATION)
    setSort((current) =>
      current === 'last_activity_desc' ? 'session_id_asc' : 'last_activity_desc',
    )
  }, [])
  const loadMore = useCallback(() => {
    const continuation = catalogPresentation.snapshot?.continuation
    if (!continuation || loadingMore || catalogRows.length >= MAX_CATALOG_ROWS) return
    setLoadingMore(true)
    setCatalogPageError(false)
    void source
      .catalogPage({ search, sort }, continuation)
      .then((page) => setCatalogPresentation((current) => appendCatalog(current, page)))
      .catch(() => setCatalogPageError(true))
      .finally(() => setLoadingMore(false))
  }, [
    catalogPresentation.snapshot?.continuation,
    catalogRows.length,
    loadingMore,
    search,
    sort,
    source,
  ])
  const controls = useMemo<SessionCommandControls>(
    () => ({
      catalogAvailable: catalogRows.length > 0,
      workspaceAvailable: sessionId !== null && catalogRows.length > 1,
      focusSearch: () => searchRef.current?.focus(),
      applySearch,
      loadMore,
      toggleSort,
      select: selectRelativeSession,
      switchSession: switchRelativeSession,
      openSelected: openSelectedSession,
    }),
    [
      applySearch,
      catalogRows.length,
      loadMore,
      openSelectedSession,
      selectRelativeSession,
      sessionId,
      switchRelativeSession,
      toggleSort,
    ],
  )
  useEffect(() => {
    onCommandControls(controls)
    return () => onCommandControls(null)
  }, [controls, onCommandControls])
  const sessionCommandContext: CommandContext = {
    dispatch,
    getState: store.getState,
    timelineIds,
    timelineWindowAvailable: session.data !== undefined,
    focusTimeline: () => timelineRef.current?.focus(),
    sessionCatalogAvailable: controls.catalogAvailable,
    sessionWorkspaceAvailable: controls.workspaceAvailable,
    focusSessionSearch: controls.focusSearch,
    applySessionSearch: controls.applySearch,
    loadMoreSessions: controls.loadMore,
    toggleSessionSort: controls.toggleSort,
    selectSession: controls.select,
    switchSession: controls.switchSession,
    openSelectedSession: controls.openSelected,
  }
  const submitSearch = (event: FormEvent) => {
    event.preventDefault()
    invokeCommand('session.catalog.apply-search', sessionCommandContext)
  }
  const handleCatalogKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const command = {
      ArrowDown: 'session.catalog.next',
      ArrowUp: 'session.catalog.previous',
      Enter: 'session.catalog.open',
    }[event.key] as
      | 'session.catalog.next'
      | 'session.catalog.previous'
      | 'session.catalog.open'
      | undefined
    if (!command) return
    event.preventDefault()
    invokeCommand(command, sessionCommandContext)
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
  const selectedSummary = catalogRows.find((summary) => summary.session_id === sessionId)

  return (
    <div className="surface-body session-workspace-surface">
      <div className="session-workbench">
        <aside className="session-catalog" aria-label="Session catalog">
          <form className="session-catalog-search" onSubmit={submitSearch}>
            <label>
              <span>Search sessions</span>
              <Search aria-hidden="true" />
              <input
                ref={searchRef}
                type="search"
                value={searchDraft}
                maxLength={256}
                onChange={(event) => setSearchDraft(event.target.value)}
                placeholder="Title or immutable ID"
              />
            </label>
            <button type="submit">Search</button>
            <button
              type="button"
              onClick={() => invokeCommand('session.catalog.sort', sessionCommandContext)}
              aria-label="Toggle session sort"
            >
              <ListFilter aria-hidden="true" />
              {sort === 'last_activity_desc' ? 'Activity' : 'Identity'}
            </button>
          </form>
          <div className="session-catalog-meta" role="status">
            <span>{catalogPresentation.snapshot?.total ?? '—'} sessions</span>
            <span>{catalogRows.length} retained</span>
            <span className={`stream-state ${catalogFollowState}`}>{catalogFollowState}</span>
          </div>
          {timelineCapability !== 'available' ? (
            <p className="session-load-state">Session catalog unavailable</p>
          ) : catalog.isError ? (
            <p className="session-load-state" role="alert">
              The daemon could not provide the bounded session catalog.
            </p>
          ) : catalog.isPending && catalogRows.length === 0 ? (
            <p className="session-load-state">Loading bounded session catalog…</p>
          ) : (
            <div
              className="session-catalog-viewport"
              ref={catalogRef}
              role="listbox"
              tabIndex={0}
              aria-label="Sessions"
              aria-activedescendant={
                selectedCatalogId ? `session-catalog-row-${selectedCatalogId}` : undefined
              }
              onKeyDown={handleCatalogKeyDown}
            >
              <div
                className="session-catalog-virtual"
                style={{ height: catalogVirtualizer.getTotalSize() }}
              >
                {catalogVirtualizer.getVirtualItems().map((virtualRow) => {
                  const summary = catalogRows[virtualRow.index]
                  if (!summary) return null
                  const isSelected = summary.session_id === selectedCatalogId
                  const isOpen = summary.session_id === sessionId
                  return (
                    <div
                      id={`session-catalog-row-${summary.session_id}`}
                      key={summary.session_id}
                      className={`${isSelected ? 'selected' : ''} ${isOpen ? 'open' : ''}`}
                      role="option"
                      aria-selected={isSelected}
                      aria-posinset={virtualRow.index + 1}
                      tabIndex={-1}
                      style={{ transform: `translateY(${virtualRow.start}px)` }}
                      onClick={() => {
                        setSelectedCatalogId(summary.session_id)
                        invokeCommand('session.catalog.open', {
                          ...sessionCommandContext,
                          openSelectedSession: () => openSessionById(summary.session_id),
                        })
                      }}
                      onKeyDown={(event) => {
                        if (event.key !== 'Enter' && event.key !== ' ') return
                        event.preventDefault()
                        setSelectedCatalogId(summary.session_id)
                        invokeCommand('session.catalog.open', {
                          ...sessionCommandContext,
                          openSelectedSession: () => openSessionById(summary.session_id),
                        })
                      }}
                    >
                      <span className={`session-state state-${summary.state}`}>
                        {stateLabel(summary.state)}
                      </span>
                      <strong>{summary.title_summary ?? summary.session_id}</strong>
                      <small>{summary.session_id}</small>
                      <span>
                        {summary.active_turn_count} active · {summary.queued_turn_count} queued
                      </span>
                      <time dateTime={activityDateTime(summary.last_activity.unix_milliseconds)}>
                        {activityLabel(summary.last_activity.unix_milliseconds)}
                      </time>
                    </div>
                  )
                })}
              </div>
            </div>
          )}
          <div className="session-catalog-footer">
            <button
              type="button"
              onClick={() => invokeCommand('session.catalog.more', sessionCommandContext)}
              disabled={
                loadingMore ||
                !catalogPresentation.snapshot?.continuation ||
                catalogRows.length >= MAX_CATALOG_ROWS
              }
            >
              {loadingMore ? 'Loading…' : 'Load more'}
            </button>
            {catalogPageError && <span role="alert">Next page unavailable</span>}
          </div>
        </aside>

        {sessionId === null ? (
          <section className="surface-empty session-entry" aria-labelledby="session-entry-heading">
            <Radio aria-hidden="true" />
            <div>
              <span
                className={`availability-tag ${timelineCapability === 'available' ? 'ready' : ''}`}
              >
                {timelineCapability === 'checking'
                  ? 'Checking session reads'
                  : timelineCapability === 'available'
                    ? 'Session reads available'
                    : 'Session reads unavailable'}
              </span>
              <h2 id="session-entry-heading">
                {timelineCapability === 'available'
                  ? 'Choose a session to open its live workspace'
                  : 'The daemon contract has not authorized session reads'}
              </h2>
              <p>
                {timelineCapability === 'available'
                  ? `Search or move with Alt+J and Alt+K, then press Enter. The catalog retains at most ${MAX_CATALOG_ROWS} rows and never opens a follow stream per row.`
                  : 'Signalbox will not call or advertise the catalog, timeline, or live follow surfaces until the exact generated capability is available.'}
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
                <span className="eyebrow">Live session workspace</span>
                <h2 id="session-workspace-heading">
                  {selectedSummary?.title_summary ?? sessionId}
                </h2>
                <small>{sessionId}</small>
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
            <div className="session-live-strip" aria-live="polite">
              <span className={`stream-state ${liveConnection}`}>
                {livePresentation.resyncing ? 'resynchronizing' : liveConnection}
              </span>
              {selectedSummary && (
                <span className={`session-state state-${selectedSummary.state}`}>
                  Session · {stateLabel(selectedSummary.state)}
                </span>
              )}
              {livePresentation.snapshot?.active && (
                <span className="typed-park">
                  Turn · {activeStateLabel(livePresentation.snapshot.active.state)}
                </span>
              )}
              {livePresentation.snapshot?.reconciliation && (
                <span className="typed-park">
                  Awaiting reconciliation ·{' '}
                  {livePresentation.snapshot.reconciliation.kind.replaceAll('_', ' ')}
                </span>
              )}
              {livePresentation.snapshot?.runner && (
                <span className={`runner-state runner-${livePresentation.snapshot.runner.state}`}>
                  {livePresentation.snapshot.runner.state.replaceAll('_', ' ')}
                </span>
              )}
              <span>{livePresentation.snapshot?.queued_turn_count ?? '0'} queued</span>
            </div>
            {livePresentation.drafts.length > 0 && (
              <section className="provider-drafts" aria-label="Non-authoritative provider draft">
                <span>Streaming draft · discarded on resync</span>
                {livePresentation.drafts.map((draft) => (
                  <p key={draft.key}>{draft.content}</p>
                ))}
              </section>
            )}
            <div className="session-window-controls" role="toolbar" aria-label="Timeline window">
              <button
                type="button"
                onClick={() =>
                  invokeCommand('selection.first', {
                    ...sessionCommandContext,
                    loadTimelineWindow: loadWindow,
                  })
                }
              >
                <SkipBack aria-hidden="true" /> First <kbd>gg</kbd>
              </button>
              <button
                type="button"
                onClick={() =>
                  invokeCommand('selection.last', {
                    ...sessionCommandContext,
                    loadTimelineWindow: loadWindow,
                  })
                }
              >
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
    </div>
  )
}
