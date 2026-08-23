import * as Dialog from '@radix-ui/react-dialog'
import { useHotkeySequences, useHotkeys } from '@tanstack/react-hotkeys'
import { useQuery } from '@tanstack/react-query'
import { Link, useNavigate } from '@tanstack/react-router'
import {
  AlertTriangle,
  Command,
  Menu,
  Moon,
  PanelLeftClose,
  Rows3,
  Search,
  Sun,
  X,
} from 'lucide-react'
import { type RefObject, useEffect, useMemo, useRef } from 'react'
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
  productTransport,
} from './product'
import { actions, selectApp, store, useAppDispatch, useAppSelector } from './state'

const surfaceCopy: Record<
  ProductRouteId,
  { eyebrow: string; title: string; question: string; track: string }
> = {
  attention: {
    eyebrow: 'Operator overview',
    title: 'Attention',
    question: 'What needs intervention now?',
    track: '#992 attention projections',
  },
  sessions: {
    eyebrow: 'Conversation index',
    title: 'Sessions',
    question: 'Where is work active, blocked, or recently settled?',
    track: '#991 session projections',
  },
  search: {
    eyebrow: 'Corpus navigation',
    title: 'Search',
    question: 'Where does this fact occur?',
    track: '#994 search reads',
  },
  activity: {
    eyebrow: 'Repository operations',
    title: 'Activity',
    question: 'What entered the system and how was it handled?',
    track: '#995 discovery reads',
  },
  runners: {
    eyebrow: 'Execution fleet',
    title: 'Runners',
    question: 'Which runners are available, occupied, or lost?',
    track: '#995 runner discovery',
  },
  reviews: {
    eyebrow: 'Convergence',
    title: 'Reviews',
    question: 'Which pull requests still need work?',
    track: '#995 review discovery',
  },
  imports: {
    eyebrow: 'Conversation intake',
    title: 'Imports',
    question: 'Which imports completed, failed, or need inspection?',
    track: '#995 import discovery',
  },
  usage: {
    eyebrow: 'Accounting',
    title: 'Usage',
    question: 'Where are tokens and cost accumulating?',
    track: '#994 usage reads',
  },
  settings: {
    eyebrow: 'Local preferences',
    title: 'Settings',
    question: 'How should this workstation present information?',
    track: 'Web track H slice 2',
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

function SurfaceUnavailable({ surface }: { surface: ProductRouteId }) {
  const copy = surfaceCopy[surface]
  return (
    <section className="surface-empty" aria-labelledby="surface-unavailable-heading">
      <AlertTriangle aria-hidden="true" />
      <div>
        <h2 id="surface-unavailable-heading">
          Operational data is not exposed by this daemon contract
        </h2>
        <p>
          The product route is ready, but {copy.track} has not supplied a production read on this
          branch. Signalbox will not infer or fabricate these facts.
        </p>
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

function SessionsSurface() {
  return (
    <div className="surface-body sessions-surface">
      <div className="sessions-toolbar" role="toolbar" aria-label="Session controls">
        <label>
          <Search aria-hidden="true" />
          <input
            aria-label="Filter loaded sessions"
            placeholder="Filter loaded sessions"
            disabled
          />
        </label>
        <button type="button" disabled>
          New session
        </button>
      </div>
      <div className="session-columns" aria-hidden="true">
        <span>Session</span>
        <span>State</span>
        <span>Activity</span>
        <span>Updated</span>
      </div>
      <SurfaceUnavailable surface="sessions" />
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

function SettingsSurface() {
  return (
    <div className="surface-body">
      <section
        className="surface-empty surface-empty-no-icon"
        aria-labelledby="settings-local-heading"
      >
        <div>
          <h2 id="settings-local-heading">Local settings are not exposed in this slice</h2>
          <p>
            Presentation preferences remain browser-local. Their dedicated controls arrive in Web
            track H slice 2 and do not depend on a daemon read contract.
          </p>
        </div>
      </section>
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
  const bootstrap = useQuery({
    queryKey: ['production', 'bootstrap'],
    queryFn: ({ signal }) => productTransport.readBootstrap(signal),
    staleTime: Number.POSITIVE_INFINITY,
  })
  const context = useMemo<CommandContext>(
    () => ({
      dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => primaryRef.current?.focus(),
      navigate: (path) => void navigate({ to: '/$surface', params: { surface: path.slice(1) } }),
      navigateScenario: () =>
        void navigate({ to: '/scenario/$scenarioId', params: { scenarioId: 'streaming' } }),
    }),
    [dispatch, navigate],
  )
  useHotkeys(
    globalHotkeyBindings.map((binding) => ({
      hotkey: binding.hotkey,
      callback: () => {
        if (store.getState().app.overlay === null) invokeCommand(binding.commandId, context)
      },
    })),
  )
  useHotkeySequences(
    globalHotkeySequenceBindings.map((binding) => ({
      sequence: binding.sequence,
      callback: () => {
        if (store.getState().app.overlay === null) invokeCommand(binding.commandId, context)
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
      document.title = 'Signalbox'
    }
  }, [surface])

  const copy = surfaceCopy[surface]
  const content =
    surface === 'attention' ? (
      <AttentionSurface />
    ) : surface === 'sessions' ? (
      <SessionsSurface />
    ) : surface === 'settings' ? (
      <SettingsSurface />
    ) : (
      <DeferredSurface surface={surface} />
    )

  return (
    <div className={`product-shell layout-${app.layout}`}>
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
            <div>
              <dt>Cache</dt>
              <dd>{surface === 'settings' ? 'Local preferences' : 'Bounded query'}</dd>
            </div>
          </dl>
        </aside>
      )}
      <CommandPalette
        context={context}
        onOpenNavigation={() => {
          navigationReturnFocusRef.current = paletteTriggerRef.current
        }}
      />
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
