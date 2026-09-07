import { useHotkeySequences, useHotkeys } from '@tanstack/react-hotkeys'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { useNavigate } from '@tanstack/react-router'
import {
  type CSSProperties,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
} from 'react'
import {
  type CommandContext,
  globalHotkeyBindings,
  globalHotkeySequenceBindings,
  invokeCommand,
} from './commands'
import { FleetTable } from './FleetTable'
import { AttachmentWorkbench } from './features/artifacts/ArtifactAttachments'
import { ArtifactWorkbench } from './features/artifacts/ArtifactRenderer'
import { artifactOriginalIds, artifactPreviewIds } from './features/artifacts/artifactScenario'
import type { WebSearchPage } from './generated/web-contract.mjs'
import {
  SCENARIO_FLEET_WINDOW_ITEMS,
  SCENARIO_TIMELINE_WINDOW_ITEMS,
  type ScenarioId,
  ScenarioTransport,
  scenarios,
} from './platform'
import type { ProductRouteId } from './product'
import { ScenarioNavigation } from './ScenarioNavigation'
import { type SearchUsageRouteState, SearchUsageWorkbench } from './SearchUsage'
import { type DiagnosticSnapshot, Diagnostics, OverlaySurfaces, Toolbar } from './Surfaces'
import {
  SEARCH_USAGE_SCENARIO_SESSION_ID,
  type SearchUsageScenarioDiagnostics,
  SearchUsageScenarioSource,
} from './search-usage/scenario'
import {
  actions,
  getRecentActions,
  selectApp,
  store,
  useAppDispatch,
  useAppSelector,
} from './state'
import { Transcript, visibleTimeline } from './Transcript'

declare global {
  interface Window {
    __SIGNALBOX_DIAGNOSTICS__?: () => DiagnosticSnapshot | undefined
    __SIGNALBOX_SEARCH_USAGE_DIAGNOSTICS__?: () => SearchUsageScenarioDiagnostics | undefined
  }
}

function useCommandHotkeys(context: CommandContext) {
  useHotkeys(
    globalHotkeyBindings.map((binding) => ({
      hotkey: binding.hotkey,
      callback: () => invokeCommand(binding.commandId, context),
    })),
  )
  useHotkeySequences(
    globalHotkeySequenceBindings.map((binding) => ({
      sequence: binding.sequence,
      callback: () => invokeCommand(binding.commandId, context),
    })),
  )
}

export function Workspace({
  scenarioId,
  route,
  onRouteChange,
}: {
  scenarioId: string
  route: SearchUsageRouteState
  onRouteChange: (patch: Partial<SearchUsageRouteState>) => void
}) {
  const primaryRef = useRef<HTMLElement>(null)
  const navigate = useNavigate()
  const knownId = scenarios.some((scenario) => scenario.id === scenarioId)
    ? (scenarioId as ScenarioId)
    : 'streaming'
  const transport = useMemo(() => new ScenarioTransport(knownId), [knownId])
  const searchUsageSource = useMemo(() => new SearchUsageScenarioSource(), [])
  const [revealedTimeline, setRevealedTimeline] = useState<Awaited<
    ReturnType<ScenarioTransport['readTimeline']>
  > | null>(null)
  const queryClient = useQueryClient()
  const queryCache = queryClient.getQueryCache()
  const queryCacheSize = useSyncExternalStore(
    (onStoreChange) => queryCache.subscribe(onStoreChange),
    () => queryCache.getAll().length,
  )
  const dispatch = useAppDispatch()
  const app = useAppSelector(selectApp)
  const timelineQuery = useQuery({
    queryKey: ['scenario', knownId, 'timeline', 'first-window'],
    queryFn: () => transport.readTimeline({ limit: SCENARIO_TIMELINE_WINDOW_ITEMS }),
    staleTime: Infinity,
  })
  const fleetQuery = useQuery({
    queryKey: ['scenario', knownId, 'fleet', 'first-window'],
    queryFn: () => transport.readFleet({ limit: SCENARIO_FLEET_WINDOW_ITEMS }),
    staleTime: Infinity,
  })
  const timeline = revealedTimeline ?? timelineQuery.data
  const fleet = fleetQuery.data
  const timelineIds = useMemo(
    () => visibleTimeline(timeline?.items ?? [], app.detail).map((item) => item.id),
    [app.detail, timeline?.items],
  )
  const firstTimelineId = timelineIds[0] ?? null
  const initialSelection = useMemo(
    () => ({ scenario: knownId, item: firstTimelineId }),
    [firstTimelineId, knownId],
  )
  const commandContext = useMemo<CommandContext>(
    () => ({
      dispatch,
      getState: store.getState,
      timelineIds,
      artifactPreviewIds: knownId === 'blobs' ? artifactPreviewIds : [],
      artifactOriginalIds: knownId === 'blobs' ? artifactOriginalIds : [],
      navigate: (path) => {
        if (path === '/scenario/streaming') {
          void navigate({
            to: '/scenario/$scenarioId',
            params: { scenarioId: 'streaming' },
          })
          return
        }
        void navigate({
          to: '/$surface',
          params: { surface: path.slice(1) as ProductRouteId },
        })
      },
      focusTimeline: () => {
        const target =
          document.querySelector<HTMLElement>('[aria-label="Session timeline"]') ??
          document.querySelector<HTMLElement>('.artifact-heading[aria-pressed="true"]') ??
          document.querySelector<HTMLElement>('.artifact-heading')
        target?.focus()
      },
      searchAvailable: knownId === 'search-usage',
      focusSearch: () => document.querySelector<HTMLInputElement>('#lexical-search-input')?.focus(),
    }),
    [dispatch, knownId, navigate, timelineIds],
  )
  useCommandHotkeys(commandContext)

  useEffect(() => {
    if (!revealedTimeline) dispatch(actions.timelineSelected(initialSelection.item))
  }, [dispatch, initialSelection, revealedTimeline])

  useEffect(() => {
    document.documentElement.dataset.theme = app.theme
    document.documentElement.dataset.density = app.density
  }, [app.density, app.theme])

  useEffect(() => {
    if (timeline !== undefined && fleet !== undefined) primaryRef.current?.focus()
  }, [fleet, timeline])

  useEffect(() => {
    document.title = `${transport.scenario.title} · Signalbox scenarios`
    return () => {
      document.title = 'Signalbox'
    }
  }, [transport.scenario.title])

  const snapshot = useMemo<DiagnosticSnapshot>(
    () => ({
      scenario: transport.scenario.id,
      connection: transport.scenario.connection,
      loadedTimeline: timeline?.items.length ?? 0,
      logicalTimeline: timeline?.totalCount ?? 0,
      loadedFleet: fleet?.items.length ?? 0,
      logicalFleet: fleet?.totalCount ?? 0,
      transcriptRange: app.transcriptRange,
      tableRange: app.tableRange,
      // Bounded diagnostic view: the workspace owns exactly two active scenario queries.
      queryStates: [
        `timeline: ${timelineQuery.status}/${timelineQuery.fetchStatus}`,
        `fleet: ${fleetQuery.status}/${fleetQuery.fetchStatus}`,
      ],
      queryCacheSize,
      recentActions: getRecentActions(),
    }),
    [
      app.tableRange,
      app.transcriptRange,
      fleet?.items.length,
      fleet?.totalCount,
      fleetQuery.fetchStatus,
      fleetQuery.status,
      queryCacheSize,
      timeline?.items.length,
      timeline?.totalCount,
      timelineQuery.fetchStatus,
      timelineQuery.status,
      transport.scenario.connection,
      transport.scenario.id,
    ],
  )
  useEffect(() => {
    window.__SIGNALBOX_DIAGNOSTICS__ = () => snapshot
    return () => {
      delete window.__SIGNALBOX_DIAGNOSTICS__
    }
  }, [snapshot])

  useEffect(() => {
    window.__SIGNALBOX_SEARCH_USAGE_DIAGNOSTICS__ =
      knownId === 'search-usage' ? () => searchUsageSource.diagnostics : () => undefined
    return () => {
      delete window.__SIGNALBOX_SEARCH_USAGE_DIAGNOSTICS__
    }
  }, [knownId, searchUsageSource])

  const revealSearchResult = async (result: WebSearchPage['results'][number]) => {
    // Global search admits hits from other sessions, but this development transport exposes exactly
    // one session's timeline and addresses it by event sequence alone. Revealing a foreign hit here
    // would select whatever unrelated evidence occupies the same address, so fail closed until a
    // per-session timeline source exists to route the reveal through.
    if (result.session_id !== SEARCH_USAGE_SCENARIO_SESSION_ID) {
      throw new TypeError('search result belongs to a session this transport cannot reveal')
    }
    const address = Number(result.address.event_sequence)
    if (!Number.isSafeInteger(address) || address < 1) {
      throw new TypeError('scenario search result address is not safely representable')
    }
    searchUsageSource.noteTranscriptReveal()
    const before = Math.max(address - 6, 0)
    const revealed = await transport.readTimeline({
      after: before > 0 ? `timeline:${before}` : undefined,
      limit: 12,
    })
    const selectedId = `event-${address}`
    if (!revealed.items.some((item) => item.id === selectedId)) {
      throw new TypeError('revealed timeline window omitted the selected search result')
    }
    setRevealedTimeline(revealed)
    dispatch(actions.timelineSelected(selectedId))
    requestAnimationFrame(() =>
      document.querySelector<HTMLElement>('[aria-label="Session timeline"]')?.focus(),
    )
  }

  if (timelineQuery.isPending || fleetQuery.isPending) {
    return (
      <main className="loading">
        <span>Loading bounded scenario windows…</span>
      </main>
    )
  }
  if (!timeline || !fleet || timelineQuery.isError || fleetQuery.isError) {
    return (
      <main className="loading" role="alert">
        Scenario transport could not provide its deterministic window.
      </main>
    )
  }

  const shellStyle = {
    '--workspace-navigation-width': `${app.paneSizes.navigation}px`,
    '--workspace-inspector-width': `${app.paneSizes.inspector}px`,
  } as CSSProperties

  return (
    <div className={`app-shell layout-${app.layout}`} style={shellStyle}>
      <aside className="navigation-pane">
        <ScenarioNavigation activeId={knownId} />
      </aside>
      <main className="workspace" tabIndex={-1} ref={primaryRef}>
        <header className="workspace-header">
          <div className="scenario-title">
            <span className={`connection connection-${transport.scenario.connection}`}>
              {transport.scenario.connection}
            </span>
            <div>
              <strong>{transport.scenario.title}</strong>
              <small>{transport.scenario.description}</small>
            </div>
          </div>
          <Toolbar context={commandContext} />
        </header>
        <div
          className={
            knownId === 'search-usage' ? 'primary-stack search-usage-stack' : 'primary-stack'
          }
        >
          {knownId === 'blobs' ? (
            <ArtifactWorkbench commandContext={commandContext} />
          ) : knownId === 'attachments' ? (
            <AttachmentWorkbench commandContext={commandContext} />
          ) : (
            <Transcript
              key={`timeline-${knownId}`}
              items={timeline.items}
              context={commandContext}
              autoFocus
            />
          )}
          {app.layout === 'workbench' && knownId === 'search-usage' && (
            <SearchUsageWorkbench
              source={searchUsageSource}
              currentSessionId={SEARCH_USAGE_SCENARIO_SESSION_ID}
              route={route}
              onRouteChange={onRouteChange}
              onReveal={revealSearchResult}
            />
          )}
          {app.layout === 'workbench' && knownId !== 'search-usage' && (
            <FleetTable key={`fleet-${knownId}`} rows={fleet.items} totalCount={fleet.totalCount} />
          )}
        </div>
      </main>
      {app.layout === 'workbench' && (
        <Diagnostics scenario={transport.scenario} snapshot={snapshot} />
      )}
      <OverlaySurfaces context={commandContext} activeId={knownId} />
    </div>
  )
}
