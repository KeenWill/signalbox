import * as Dialog from '@radix-ui/react-dialog'
import { useHotkeySequences, useHotkeys } from '@tanstack/react-hotkeys'
import { useQuery } from '@tanstack/react-query'
import { Link, useNavigate } from '@tanstack/react-router'
import { AlertTriangle, Command, Menu, Moon, PanelLeftClose, Rows3, Sun, X } from 'lucide-react'
import {
  type CSSProperties,
  type RefObject,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react'
import {
  type CommandContext,
  type CommandId,
  commandRegistry,
  globalHotkeyBindings,
  globalHotkeySequenceBindings,
  invokeCommand,
} from './commands'
import {
  BootstrapContractError,
  type ProductRouteId,
  productRoutes,
  productSurfaceCacheLabel,
  productSurfaceStates,
  productTransport,
} from './product'
import {
  type SessionCommandControls,
  type SessionSelectionEvidence,
  SessionWorkspaceSurface,
} from './SessionWorkspaceSurface'
import { SettingsSurface } from './SettingsSurface'
import { actions, selectApp, store, useAppDispatch, useAppSelector } from './state'

const surfaceCopy: Record<ProductRouteId, { eyebrow: string; title: string; question: string }> = {
  attention: {
    eyebrow: 'Operator overview',
    title: 'Attention',
    question: 'What needs intervention now?',
  },
  sessions: {
    eyebrow: 'Conversation index',
    title: 'Sessions',
    question: 'Where is work active, blocked, or recently settled?',
  },
  search: {
    eyebrow: 'Corpus navigation',
    title: 'Search',
    question: 'Where does this fact occur?',
  },
  activity: {
    eyebrow: 'Repository operations',
    title: 'Activity',
    question: 'What entered the system and how was it handled?',
  },
  runners: {
    eyebrow: 'Execution fleet',
    title: 'Runners',
    question: 'Which runners are available, occupied, or lost?',
  },
  reviews: {
    eyebrow: 'Convergence',
    title: 'Reviews',
    question: 'Which pull requests still need work?',
  },
  imports: {
    eyebrow: 'Conversation intake',
    title: 'Imports',
    question: 'Which imports completed, failed, or need inspection?',
  },
  usage: {
    eyebrow: 'Accounting',
    title: 'Usage',
    question: 'Where are tokens and cost accumulating?',
  },
  settings: {
    eyebrow: 'Local preferences',
    title: 'Settings',
    question: 'How should this workstation present information?',
  },
}

const productNavigationCommandIds: Record<ProductRouteId, CommandId> = {
  attention: 'navigate.attention',
  sessions: 'navigate.sessions',
  search: 'navigate.search',
  activity: 'navigate.activity',
  runners: 'navigate.runners',
  reviews: 'navigate.reviews',
  imports: 'navigate.imports',
  usage: 'navigate.usage',
  settings: 'navigate.settings',
}

function ProductNavigation({
  active,
  context,
  onNavigate,
}: {
  active: ProductRouteId
  context: CommandContext
  onNavigate?: () => void
}) {
  return (
    <div className="product-navigation">
      <div className="brand">
        <span className="brand-mark">SB</span>
        <strong>Signalbox</strong>
        <small>Operator workstation</small>
      </div>
      <nav aria-label="Product">
        {productRoutes.map((route) => (
          <Link
            key={route.id}
            to="/$surface"
            params={{ surface: route.id }}
            className={active === route.id ? 'product-link active' : 'product-link'}
            aria-current={active === route.id ? 'page' : undefined}
            onClick={(event) => {
              if (
                event.button === 0 &&
                !event.altKey &&
                !event.ctrlKey &&
                !event.metaKey &&
                !event.shiftKey
              ) {
                event.preventDefault()
                onNavigate?.()
                invokeCommand(productNavigationCommandIds[route.id], context)
              }
            }}
          >
            <span>{route.label}</span>
            <small>{route.description}</small>
          </Link>
        ))}
      </nav>
      <Link
        className="scenario-entry"
        to="/scenario/$scenarioId"
        params={{ scenarioId: 'streaming' }}
        onClick={(event) => {
          if (
            event.button === 0 &&
            !event.altKey &&
            !event.ctrlKey &&
            !event.metaKey &&
            !event.shiftKey
          ) {
            event.preventDefault()
            onNavigate?.()
            invokeCommand('navigate.scenario', context)
          }
        }}
      >
        Scenario studio <span aria-hidden="true">↗</span>
      </Link>
    </div>
  )
}

function CommandPalette({
  context,
  onOpenNavigation,
}: {
  context: CommandContext
  onOpenNavigation: () => void
}) {
  const open = useAppSelector((state) => state.app.overlay === 'palette')
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        if (!next) invokeCommand('surface.escape', context)
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className="dialog-overlay" />
        <Dialog.Content
          className="dialog-content product-palette"
          aria-describedby="product-palette-description"
        >
          <div className="dialog-heading">
            <div>
              <Dialog.Title>Command palette</Dialog.Title>
              <Dialog.Description id="product-palette-description">
                Navigate and adjust the workstation from one command registry.
              </Dialog.Description>
            </div>
            <Dialog.Close asChild>
              <button className="icon-button" type="button" aria-label="Close command palette">
                <X />
              </button>
            </Dialog.Close>
          </div>
          <div className="command-list">
            {commandRegistry
              .filter((command) => command.id !== 'surface.escape' && command.available(context))
              .map((command) => (
                <button
                  key={command.id}
                  type="button"
                  onClick={() => {
                    invokeCommand('surface.escape', context)
                    if (command.id === 'navigation.open') onOpenNavigation()
                    invokeCommand(command.id, context)
                  }}
                >
                  <span>
                    <strong>{command.title}</strong>
                    <small>{command.description}</small>
                  </span>
                  <kbd>{command.bindings[0]?.label ?? '—'}</kbd>
                </button>
              ))}
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  )
}

function KeyboardHelp({ context }: { context: CommandContext }) {
  const open = useAppSelector((state) => state.app.overlay === 'help')
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        if (!next) invokeCommand('surface.escape', context)
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className="dialog-overlay" />
        <Dialog.Content
          className="dialog-content product-palette"
          aria-describedby="keyboard-help-description"
        >
          <div className="dialog-heading">
            <div>
              <Dialog.Title>Keyboard help</Dialog.Title>
              <Dialog.Description id="keyboard-help-description">
                Available workstation commands and bindings.
              </Dialog.Description>
            </div>
            <Dialog.Close asChild>
              <button className="icon-button" type="button" aria-label="Close keyboard help">
                <X />
              </button>
            </Dialog.Close>
          </div>
          <div className="command-list">
            {commandRegistry
              .filter(
                (command) =>
                  command.id !== 'surface.escape' &&
                  command.bindings.length > 0 &&
                  command.available(context),
              )
              .map((command) => (
                <div key={command.id}>
                  <span>
                    <strong>{command.title}</strong>
                    <small>{command.description}</small>
                  </span>
                  <kbd>{command.bindings.map((binding) => binding.label).join(' / ')}</kbd>
                </div>
              ))}
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  )
}

function SurfaceUnavailable({ surface }: { surface: ProductRouteId }) {
  const state = productSurfaceStates[surface]
  if (state.kind !== 'committed-unimplemented') return null
  return (
    <section className="surface-empty" aria-labelledby={`${surface}-unavailable-heading`}>
      <AlertTriangle aria-hidden="true" />
      <div>
        <span className="availability-tag">Committed · unavailable</span>
        <h2 id={`${surface}-unavailable-heading`}>
          Operational data is not exposed by this daemon contract
        </h2>
        <p>
          {state.owningTrack} is committed, but no present production surface provides the required
          facts on this branch. Signalbox will not infer or fabricate them.
        </p>
        <ul>
          {state.facts.map((fact) => (
            <li key={fact}>{fact}</li>
          ))}
        </ul>
      </div>
    </section>
  )
}

function AttentionSurface() {
  return (
    <div className="surface-body attention-surface">
      <section className="surface-intro">
        <span className="eyebrow">Decision queue</span>
        <h2>Intervention before observation</h2>
        <p>
          Approvals, blocked goals, ambiguous outcomes, runner loss, and held repository work will
          share one bounded priority surface when their owning read model is available.
        </p>
      </section>
      <SurfaceUnavailable surface="attention" />
    </div>
  )
}

function DeferredSurface({ surface }: { surface: ProductRouteId }) {
  return (
    <div className="surface-body">
      <SurfaceUnavailable surface={surface} />
    </div>
  )
}

function ProductToolbar({
  context,
  navigationTriggerRef,
  paletteTriggerRef,
  onOpenNavigation,
}: {
  context: CommandContext
  navigationTriggerRef: RefObject<HTMLButtonElement | null>
  paletteTriggerRef: RefObject<HTMLButtonElement | null>
  onOpenNavigation: () => void
}) {
  const app = useAppSelector(selectApp)
  return (
    <div className="toolbar" role="toolbar" aria-label="Application controls">
      <button
        ref={navigationTriggerRef}
        className="icon-button mobile-only"
        type="button"
        aria-label="Open navigation"
        onClick={() => {
          onOpenNavigation()
          invokeCommand('navigation.open', context)
        }}
      >
        <Menu />
      </button>
      <button
        className="icon-button"
        type="button"
        aria-label={`Use ${app.density === 'compact' ? 'comfortable' : 'compact'} density`}
        onClick={() => invokeCommand('density.toggle', context)}
      >
        <Rows3 />
      </button>
      <button
        className="icon-button"
        type="button"
        aria-label={`Switch to ${app.layout === 'focus' ? 'workbench' : 'focus'} layout`}
        onClick={() => invokeCommand('layout.toggle', context)}
      >
        <PanelLeftClose />
      </button>
      <button
        className="icon-button"
        type="button"
        aria-label={`Use ${app.theme === 'dark' ? 'light' : 'dark'} theme`}
        onClick={() => invokeCommand('theme.toggle', context)}
      >
        {app.theme === 'dark' ? <Sun /> : <Moon />}
      </button>
      <button
        ref={paletteTriggerRef}
        className="icon-button"
        type="button"
        aria-label="Open command palette"
        onClick={() => invokeCommand('palette.open', context)}
      >
        <Command />
      </button>
    </div>
  )
}

export function ProductApp({ surface }: { surface: ProductRouteId }) {
  const dispatch = useAppDispatch()
  const app = useAppSelector(selectApp)
  const navigate = useNavigate()
  const primaryRef = useRef<HTMLElement>(null)
  const navigationTriggerRef = useRef<HTMLButtonElement>(null)
  const paletteTriggerRef = useRef<HTMLButtonElement>(null)
  const navigationReturnFocusRef = useRef<HTMLButtonElement | null>(null)
  const timelineRef = useRef<HTMLDivElement>(null)
  const [timelineIds, setTimelineIds] = useState<readonly string[]>([])
  const [sessionControls, setSessionControls] = useState<SessionCommandControls | null>(null)
  const [timelineWindowAvailable, setTimelineWindowAvailable] = useState(false)
  const [selectionEvidence, setSelectionEvidence] = useState<SessionSelectionEvidence | null>(null)
  const [windowRequest, setWindowRequest] = useState<{
    anchor: 'first' | 'latest'
    attempt: number
  } | null>(null)
  const updateTimelineIds = useCallback((ids: readonly string[]) => setTimelineIds(ids), [])
  const updateSelectionEvidence = useCallback(
    (evidence: SessionSelectionEvidence | null) => setSelectionEvidence(evidence),
    [],
  )
  const consumeWindowRequest = useCallback(() => setWindowRequest(null), [])
  const bootstrap = useQuery({
    queryKey: ['production', 'bootstrap'],
    queryFn: ({ signal }) => productTransport.readBootstrap(signal),
    staleTime: Number.POSITIVE_INFINITY,
  })
  const context = useMemo<CommandContext>(
    () => ({
      dispatch,
      getState: store.getState,
      timelineIds,
      timelineWindowAvailable: surface === 'sessions' && timelineWindowAvailable,
      focusTimeline: () => (timelineRef.current ?? primaryRef.current)?.focus(),
      loadTimelineWindow: (anchor) =>
        setWindowRequest((current) => ({ anchor, attempt: (current?.attempt ?? 0) + 1 })),
      navigate: (path) => void navigate({ to: '/$surface', params: { surface: path.slice(1) } }),
      navigateScenario: () =>
        void navigate({ to: '/scenario/$scenarioId', params: { scenarioId: 'streaming' } }),
      sessionCatalogAvailable: surface === 'sessions' && sessionControls?.catalogAvailable === true,
      sessionWorkspaceAvailable:
        surface === 'sessions' && sessionControls?.workspaceAvailable === true,
      focusSessionSearch: surface === 'sessions' ? sessionControls?.focusSearch : undefined,
      applySessionSearch: surface === 'sessions' ? sessionControls?.applySearch : undefined,
      loadMoreSessions: surface === 'sessions' ? sessionControls?.loadMore : undefined,
      toggleSessionSort: surface === 'sessions' ? sessionControls?.toggleSort : undefined,
      selectSession: surface === 'sessions' ? sessionControls?.select : undefined,
      switchSession: surface === 'sessions' ? sessionControls?.switchSession : undefined,
      openSelectedSession: surface === 'sessions' ? sessionControls?.openSelected : undefined,
    }),
    [dispatch, navigate, sessionControls, surface, timelineIds, timelineWindowAvailable],
  )
  useHotkeys(
    globalHotkeyBindings.map((binding) => ({
      hotkey: binding.hotkey,
      callback: () => {
        if (store.getState().app.overlay === null || binding.commandId === 'surface.escape') {
          if (binding.commandId.startsWith('selection.')) context.focusTimeline()
          invokeCommand(binding.commandId, context)
        }
      },
    })),
  )
  useHotkeySequences(
    globalHotkeySequenceBindings.map((binding) => ({
      sequence: binding.sequence,
      callback: () => {
        if (store.getState().app.overlay === null) {
          if (binding.commandId.startsWith('selection.')) context.focusTimeline()
          invokeCommand(binding.commandId, context)
        }
      },
    })),
  )

  useEffect(() => {
    if (store.getState().app.overlay === 'help') dispatch(actions.overlaySet(null))
  }, [dispatch])

  useEffect(() => {
    document.documentElement.dataset.theme = app.theme
    document.documentElement.dataset.density = app.density
  }, [app.density, app.theme])

  useEffect(() => {
    document.title = `${surfaceCopy[surface].title} · Signalbox`
    return () => {
      document.title = 'Signalbox scenarios'
    }
  }, [surface])

  const copy = surfaceCopy[surface]
  const cacheLabel = productSurfaceCacheLabel(surface)
  const timelineCapability = bootstrap.isPending
    ? 'checking'
    : bootstrap.isSuccess &&
        bootstrap.data.capabilities.bounded_session_timeline &&
        bootstrap.data.capabilities.bounded_session_live
      ? 'available'
      : 'unavailable'

  const content =
    surface === 'attention' ? (
      <AttentionSurface />
    ) : surface === 'sessions' ? (
      <SessionWorkspaceSurface
        maxNdjsonRecordBytes={bootstrap.data?.limits.max_ndjson_item_bytes ?? 65_536}
        onCommandControls={setSessionControls}
        onSelectionEvidence={updateSelectionEvidence}
        onTimelineIds={updateTimelineIds}
        onTimelineWindowAvailable={setTimelineWindowAvailable}
        onWindowRequestConsumed={consumeWindowRequest}
        timelineCapability={timelineCapability}
        timelineRef={timelineRef}
        windowRequest={windowRequest}
      />
    ) : surface === 'settings' ? (
      <SettingsSurface />
    ) : (
      <DeferredSurface surface={surface} />
    )

  const shellStyle = {
    '--product-navigation-width': `${app.paneSizes.navigation}px`,
    '--product-inspector-width': `${app.paneSizes.inspector}px`,
  } as CSSProperties

  return (
    <div className={`product-shell layout-${app.layout}`} style={shellStyle}>
      <aside className="product-navigation-pane">
        <ProductNavigation active={surface} context={context} />
      </aside>
      <main className="product-main" tabIndex={-1} ref={primaryRef}>
        <header className="product-header">
          <div>
            <span className="eyebrow">{copy.eyebrow}</span>
            <h1>{copy.title}</h1>
          </div>
          <ProductToolbar
            context={context}
            navigationTriggerRef={navigationTriggerRef}
            paletteTriggerRef={paletteTriggerRef}
            onOpenNavigation={() => {
              navigationReturnFocusRef.current = navigationTriggerRef.current
            }}
          />
        </header>
        <div className="surface-question">
          <p>{copy.question}</p>
          <span
            className={`contract-state ${
              surface === 'settings' || bootstrap.isSuccess
                ? 'ready'
                : bootstrap.isError
                  ? 'failed'
                  : ''
            }`}
            role="status"
            aria-live="polite"
          >
            {surface === 'settings'
              ? 'Browser-local preferences'
              : bootstrap.isSuccess
                ? `${bootstrap.data.contract.name} · ${bootstrap.data.contract.version}`
                : bootstrap.isError
                  ? bootstrap.error instanceof BootstrapContractError
                    ? 'Incompatible daemon contract'
                    : 'Transport unavailable'
                  : 'Checking contract…'}
          </span>
          {surface !== 'settings' && bootstrap.isError && (
            <button
              type="button"
              className="bootstrap-retry"
              onClick={() => void bootstrap.refetch()}
            >
              Retry contract
            </button>
          )}
        </div>
        {content}
      </main>
      {app.layout === 'workbench' && (
        <aside className="product-inspector" aria-label="Inspector">
          <span className="eyebrow">Inspector</span>
          <h2>Selection details</h2>
          <p>
            {surface === 'settings'
              ? 'Presentation preferences are stored locally in this browser.'
              : surface === 'sessions' && selectionEvidence !== null
                ? 'Bounded server-provided timeline projection for the selected record.'
                : 'Select an available operational record to inspect its server-provided evidence.'}
          </p>
          <dl>
            <div>
              <dt>Surface</dt>
              <dd>{copy.title}</dd>
            </div>
            <div>
              <dt>Authority</dt>
              <dd>{surface === 'settings' ? 'Browser' : 'Daemon'}</dd>
            </div>
            {cacheLabel !== null && (
              <div>
                <dt>Cache</dt>
                <dd>{cacheLabel}</dd>
              </div>
            )}
            {surface === 'sessions' && selectionEvidence !== null && (
              <>
                <div>
                  <dt>Session</dt>
                  <dd>{selectionEvidence.sessionId}</dd>
                </div>
                <div>
                  <dt>Event</dt>
                  <dd>{selectionEvidence.eventSequence}</dd>
                </div>
                <div>
                  <dt>Kind</dt>
                  <dd>{selectionEvidence.kind.replaceAll('_', ' ')}</dd>
                </div>
                <div>
                  <dt>Projected bytes</dt>
                  <dd>{selectionEvidence.projectedStructuredBytes}</dd>
                </div>
              </>
            )}
          </dl>
        </aside>
      )}
      <CommandPalette
        context={context}
        onOpenNavigation={() => {
          navigationReturnFocusRef.current = paletteTriggerRef.current
        }}
      />
      <KeyboardHelp context={context} />
      <Dialog.Root
        open={app.overlay === 'navigation'}
        onOpenChange={(open) => {
          if (!open) invokeCommand('surface.escape', context)
        }}
      >
        <Dialog.Portal>
          <Dialog.Overlay className="dialog-overlay" />
          <Dialog.Content
            className="mobile-navigation"
            aria-describedby="mobile-navigation-description"
            onCloseAutoFocus={(event) => {
              const trigger = navigationReturnFocusRef.current
              if (trigger?.getClientRects().length) {
                event.preventDefault()
                trigger.focus()
              }
            }}
          >
            <Dialog.Title className="sr-only">Product navigation</Dialog.Title>
            <Dialog.Description id="mobile-navigation-description" className="sr-only">
              Choose a Signalbox surface.
            </Dialog.Description>
            <ProductNavigation
              active={surface}
              context={context}
              onNavigate={() => invokeCommand('surface.escape', context)}
            />
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
    </div>
  )
}
