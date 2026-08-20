import { useHotkey, useHotkeySequence } from '@tanstack/react-hotkeys'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { useEffect, useMemo } from 'react'
import { FleetTable } from './FleetTable'
import { invokeCommand, type CommandContext, type CommandId } from './commands'
import { ScenarioNavigation } from './ScenarioNavigation'
import {
  SCENARIO_FLEET_WINDOW_ITEMS,
  SCENARIO_TIMELINE_WINDOW_ITEMS,
  ScenarioTransport,
  scenarios,
  type ScenarioId,
} from './platform'
import { store, actions, getRecentActions, selectApp, useAppDispatch, useAppSelector } from './state'
import { Diagnostics, OverlaySurfaces, Toolbar, type DiagnosticSnapshot } from './Surfaces'
import { Transcript, visibleTimeline } from './Transcript'

declare global {
  interface Window {
    __SIGNALBOX_DIAGNOSTICS__?: () => DiagnosticSnapshot | undefined
  }
}

// Hard safety ceiling: the debug endpoint never walks the full Query cache.
const DIAGNOSTIC_QUERY_STATES = 8

function useCommandHotkeys(context: CommandContext) {
  const run = (id: CommandId) => invokeCommand(id, context)
  useHotkey('Mod+K', () => run('palette.open'))
  useHotkey({ key: '/', shift: true }, () => run('help.open'))
  useHotkey('J', () => run('selection.next'))
  useHotkey('K', () => run('selection.previous'))
  useHotkey('Shift+G', () => run('selection.last'))
  useHotkey('Shift+W', () => run('layout.toggle'))
  useHotkey('Shift+D', () => run('density.toggle'))
  useHotkey('Shift+T', () => run('theme.toggle'))
  useHotkey('Escape', () => run('surface.escape'))
  useHotkeySequence(['G', 'G'], () => run('selection.first'))
}

export function Workspace({ scenarioId }: { scenarioId: string }) {
  const knownId = scenarios.some((scenario) => scenario.id === scenarioId) ? scenarioId as ScenarioId : 'streaming'
  const transport = useMemo(() => new ScenarioTransport(knownId), [knownId])
  const queryClient = useQueryClient()
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
  const timeline = timelineQuery.data
  const fleet = fleetQuery.data
  const visibleCount = visibleTimeline(timeline?.items ?? [], app.detail).length
  const commandContext = useMemo<CommandContext>(() => ({
    dispatch,
    getState: store.getState,
    timelineCount: visibleCount,
    focusTimeline: () => {
      const active = document.activeElement
      if (active instanceof HTMLElement) active.blur()
      document.querySelector<HTMLElement>('[aria-label="Session timeline"]')?.focus()
    },
  }), [dispatch, visibleCount])
  useCommandHotkeys(commandContext)

  useEffect(() => {
    dispatch(actions.timelineSelected(0))
  }, [dispatch, knownId])

  useEffect(() => {
    document.documentElement.dataset.theme = app.theme
    document.documentElement.dataset.density = app.density
  }, [app.density, app.theme])

  const snapshot: DiagnosticSnapshot = {
    scenario: transport.scenario.id,
    connection: transport.scenario.connection,
    loadedTimeline: timeline?.items.length ?? 0,
    logicalTimeline: timeline?.totalCount ?? 0,
    loadedFleet: fleet?.items.length ?? 0,
    logicalFleet: fleet?.totalCount ?? 0,
    transcriptRange: app.transcriptRange,
    tableRange: app.tableRange,
    queryStates: queryClient.getQueryCache().getAll().slice(-DIAGNOSTIC_QUERY_STATES).map((query) => `${query.queryHash}: ${query.state.status}`),
    recentActions: getRecentActions(),
  }
  useEffect(() => {
    window.__SIGNALBOX_DIAGNOSTICS__ = () => snapshot
    return () => { delete window.__SIGNALBOX_DIAGNOSTICS__ }
  }, [snapshot])

  if (timelineQuery.isPending || fleetQuery.isPending) {
    return <main className="loading"><span>Loading bounded scenario windows…</span></main>
  }
  if (!timeline || !fleet || timelineQuery.isError || fleetQuery.isError) {
    return <main className="loading" role="alert">Scenario transport could not provide its deterministic window.</main>
  }

  return (
    <div className={`app-shell layout-${app.layout}`}>
      <aside className="navigation-pane"><ScenarioNavigation activeId={knownId} /></aside>
      <main className="workspace">
        <header className="workspace-header">
          <div className="scenario-title">
            <span className={`connection connection-${transport.scenario.connection}`}>{transport.scenario.connection}</span>
            <div><strong>{transport.scenario.title}</strong><small>{transport.scenario.description}</small></div>
          </div>
          <Toolbar context={commandContext} />
        </header>
        <div className="primary-stack">
          <Transcript items={timeline.items} />
          {app.layout === 'workbench' && <FleetTable rows={fleet.items} totalCount={fleet.totalCount} />}
        </div>
      </main>
      {app.layout === 'workbench' && (
        <Diagnostics
          scenario={transport.scenario}
          snapshot={snapshot}
        />
      )}
      <OverlaySurfaces context={commandContext} activeId={knownId} />
    </div>
  )
}
