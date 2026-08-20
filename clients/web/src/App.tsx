import { useHotkey, useHotkeySequence } from '@tanstack/react-hotkeys'
import { useQuery } from '@tanstack/react-query'
import { useEffect, useMemo } from 'react'
import { type CommandContext, type CommandId, invokeCommand } from './commands'
import { FleetTable } from './FleetTable'
import {
  SCENARIO_FLEET_WINDOW_ITEMS,
  SCENARIO_TIMELINE_WINDOW_ITEMS,
  type ScenarioId,
  ScenarioTransport,
  scenarios,
} from './platform'
import { ScenarioNavigation } from './ScenarioNavigation'
import { type DiagnosticSnapshot, Diagnostics, OverlaySurfaces, Toolbar } from './Surfaces'
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
  }
}

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
  const knownId = scenarios.some((scenario) => scenario.id === scenarioId)
    ? (scenarioId as ScenarioId)
    : 'streaming'
  const transport = useMemo(() => new ScenarioTransport(knownId), [knownId])
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
      focusTimeline: () => {
        const active = document.activeElement
        if (active instanceof HTMLElement) active.blur()
        document.querySelector<HTMLElement>('[aria-label="Session timeline"]')?.focus()
      },
    }),
    [dispatch, timelineIds],
  )
  useCommandHotkeys(commandContext)

  useEffect(() => {
    dispatch(actions.timelineSelected(initialSelection.item))
  }, [dispatch, initialSelection])

  useEffect(() => {
    document.documentElement.dataset.theme = app.theme
    document.documentElement.dataset.density = app.density
  }, [app.density, app.theme])

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
      // Hard safety ceiling: diagnostics expose only the two active bounded queries.
      queryStates: [
        `timeline: ${timelineQuery.status}/${timelineQuery.fetchStatus}`,
        `fleet: ${fleetQuery.status}/${fleetQuery.fetchStatus}`,
      ],
      recentActions: getRecentActions(),
    }),
    [
      app.tableRange,
      app.transcriptRange,
      fleet?.items.length,
      fleet?.totalCount,
      fleetQuery.fetchStatus,
      fleetQuery.status,
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

  return (
    <div className={`app-shell layout-${app.layout}`}>
      <aside className="navigation-pane">
        <ScenarioNavigation activeId={knownId} />
      </aside>
      <main className="workspace">
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
        <div className="primary-stack">
          <Transcript items={timeline.items} context={commandContext} />
          {app.layout === 'workbench' && (
            <FleetTable rows={fleet.items} totalCount={fleet.totalCount} />
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
