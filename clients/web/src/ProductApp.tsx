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
import { type CSSProperties, useCallback, useMemo, useRef, useState } from 'react'
import {
  type CommandContext,
  commandRegistry,
  globalHotkeyBindings,
  globalHotkeySequenceBindings,
  invokeCommand,
} from './commands'
import { HttpImportApi } from './imports/api'
import { ImportsWorkspace } from './imports/ImportsWorkspace'
import {
  type ProductRouteId,
  productRoutes,
  productSurfaceStates,
  productTransport,
} from './product'
import { SettingsSurface } from './SettingsSurface'
import { actions, selectApp, store, useAppDispatch, useAppSelector } from './state'

const productImportApi = new HttpImportApi()

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

function ProductNavigation({
  active,
  disabled = false,
  onNavigate,
}: {
  active: ProductRouteId
  disabled?: boolean
  onNavigate?: (destination: 'product' | 'scenario') => void
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
            aria-disabled={disabled || undefined}
            tabIndex={disabled ? -1 : undefined}
            onClick={(event) => {
              if (disabled) event.preventDefault()
              else onNavigate?.('product')
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
        aria-disabled={disabled || undefined}
        tabIndex={disabled ? -1 : undefined}
        onClick={(event) => {
          if (disabled) event.preventDefault()
          else onNavigate?.('scenario')
        }}
      >
        Scenario studio <span aria-hidden="true">↗</span>
      </Link>
    </div>
  )
}

function CommandPalette({ context }: { context: CommandContext }) {
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

function ProductToolbar({ context }: { context: CommandContext }) {
  const app = useAppSelector(selectApp)
  return (
    <div className="toolbar" role="toolbar" aria-label="Application controls">
      <button
        className="icon-button mobile-only"
        type="button"
        aria-label="Open navigation"
        onClick={() => invokeCommand('navigation.open', context)}
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
  const restoreNavigationFocusRef = useRef(true)
  const [importsCommandContext, setImportsCommandContext] = useState<CommandContext | null>(null)
  const [navigationDisabled, setNavigationDisabled] = useState(false)
  const updateImportsCommandContext = useCallback(
    (nextContext: CommandContext | null) => setImportsCommandContext(nextContext),
    [],
  )
  const bootstrap = useQuery({
    queryKey: ['production', 'bootstrap'],
    queryFn: ({ signal }) => productTransport.readBootstrap(signal),
    staleTime: Number.POSITIVE_INFINITY,
  })
  const context = useMemo<CommandContext>(() => {
    const surfaceContext = surface === 'imports' ? importsCommandContext : null
    return {
      ...surfaceContext,
      dispatch,
      getState: store.getState,
      timelineIds: surfaceContext?.timelineIds ?? [],
      focusTimeline: surfaceContext?.focusTimeline ?? (() => primaryRef.current?.focus()),
      navigate: navigationDisabled
        ? undefined
        : (path) => void navigate({ to: '/$surface', params: { surface: path.slice(1) } }),
    }
  }, [dispatch, importsCommandContext, navigate, navigationDisabled, surface])
  useHotkeys(
    globalHotkeyBindings.map((binding) => ({
      hotkey: binding.hotkey,
      callback: () => invokeCommand(binding.commandId, context),
      options: { enabled: app.overlay === null },
    })),
  )
  useHotkeySequences(
    globalHotkeySequenceBindings.map((binding) => ({
      sequence: binding.sequence,
      callback: () => invokeCommand(binding.commandId, context),
      options: { enabled: app.overlay === null },
    })),
  )

  const copy = surfaceCopy[surface]
  const content =
    surface === 'attention' ? (
      <AttentionSurface />
    ) : surface === 'sessions' ? (
      <SessionsSurface />
    ) : surface === 'settings' ? (
      <SettingsSurface />
    ) : surface === 'imports' ? (
      <ImportsWorkspace
        api={productImportApi}
        scenario={false}
        presentation="product"
        onCommandContext={updateImportsCommandContext}
        onNavigationDisabledChange={setNavigationDisabled}
      />
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
        <ProductNavigation active={surface} disabled={navigationDisabled} />
      </aside>
      <main className={`product-main product-main-${surface}`} tabIndex={-1} ref={primaryRef}>
        <header className="product-header">
          <div>
            <span className="eyebrow">{copy.eyebrow}</span>
            <h1>{copy.title}</h1>
          </div>
          <ProductToolbar context={context} />
        </header>
        <div className="surface-question">
          <p>{copy.question}</p>
          <span
            className={`contract-state ${bootstrap.isSuccess ? 'ready' : bootstrap.isError ? 'failed' : ''}`}
            role="status"
            aria-atomic="true"
          >
            {bootstrap.isSuccess
              ? `${bootstrap.data.contract.name} · ${bootstrap.data.contract.version}`
              : bootstrap.isError
                ? 'Transport unavailable'
                : 'Checking contract…'}
          </span>
        </div>
        {content}
      </main>
      {app.layout === 'workbench' && (
        <aside className="product-inspector" aria-label="Inspector">
          <span className="eyebrow">Inspector</span>
          <h2>Selection details</h2>
          <p>Select an available operational record to inspect its server-provided evidence.</p>
          <dl>
            <div>
              <dt>Surface</dt>
              <dd>{copy.title}</dd>
            </div>
            <div>
              <dt>Authority</dt>
              <dd>
                {productSurfaceStates[surface].kind === 'browser-local'
                  ? 'Browser'
                  : productSurfaceStates[surface].kind === 'server-backed'
                    ? 'Daemon'
                    : 'Unavailable'}
              </dd>
            </div>
            <div>
              <dt>Cache</dt>
              <dd>
                {productSurfaceStates[surface].kind === 'browser-local'
                  ? 'Local settings'
                  : productSurfaceStates[surface].kind === 'server-backed'
                    ? 'Bounded query'
                    : 'None'}
              </dd>
            </div>
          </dl>
        </aside>
      )}
      <CommandPalette context={context} />
      <Dialog.Root
        open={app.overlay === 'navigation'}
        onOpenChange={(open) => {
          if (!open) dispatch(actions.overlaySet(null))
        }}
      >
        <Dialog.Portal>
          <Dialog.Overlay className="dialog-overlay" />
          <Dialog.Content
            className="mobile-navigation"
            aria-describedby="mobile-navigation-description"
            onCloseAutoFocus={(event) => {
              const returnTarget = document.querySelector<HTMLElement>(
                '[aria-label="Open navigation"]',
              )
              if (restoreNavigationFocusRef.current && returnTarget) {
                event.preventDefault()
                returnTarget.focus()
              }
            }}
          >
            <Dialog.Title className="sr-only">Product navigation</Dialog.Title>
            <Dialog.Description id="mobile-navigation-description" className="sr-only">
              Choose a Signalbox surface.
            </Dialog.Description>
            <ProductNavigation
              active={surface}
              disabled={navigationDisabled}
              onNavigate={(destination) => {
                restoreNavigationFocusRef.current = destination === 'product'
                dispatch(actions.overlaySet(null))
              }}
            />
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
    </div>
  )
}
