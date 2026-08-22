import * as Dialog from '@radix-ui/react-dialog'
import { useHotkeySequences, useHotkeys } from '@tanstack/react-hotkeys'
import { useQuery } from '@tanstack/react-query'
import { Link, useNavigate } from '@tanstack/react-router'
import { AlertTriangle, Command, Menu, Moon, PanelLeftClose, Rows3, Sun, X } from 'lucide-react'
import { type CSSProperties, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  type ProductRouteId,
  productRoutes,
  productSurfaceCacheLabel,
  productSurfaceStates,
  productTransport,
} from './product'
import {
  invokeProductCommand,
  type ProductCommandContext,
  productCommandRegistry,
  productHotkeyBindings,
  productHotkeySequenceBindings,
} from './productCommands'
import { SessionWorkspaceSurface } from './SessionWorkspaceSurface'
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

function ProductNavigation({
  active,
  onActivate,
}: {
  active: ProductRouteId
  onActivate?: () => void
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
            onClick={onActivate}
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
        onClick={onActivate}
      >
        Scenario studio <span aria-hidden="true">↗</span>
      </Link>
    </div>
  )
}

function CommandPalette({ context }: { context: ProductCommandContext }) {
  const open = useAppSelector((state) => state.app.overlay === 'palette')
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        if (!next) invokeProductCommand('surface.escape', context)
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
            {productCommandRegistry
              .filter(
                (command) =>
                  command.id !== 'surface.escape' &&
                  (!('available' in command) || command.available(context)),
              )
              .map((command) => (
                <button
                  key={command.id}
                  type="button"
                  onClick={() => {
                    invokeProductCommand('surface.escape', context)
                    invokeProductCommand(command.id, context)
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

function KeyboardHelp({ context }: { context: ProductCommandContext }) {
  const open = useAppSelector((state) => state.app.overlay === 'help')
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        if (!next) invokeProductCommand('surface.escape', context)
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
            {productCommandRegistry
              .filter(
                (command) =>
                  command.id !== 'surface.escape' &&
                  command.bindings.length > 0 &&
                  (!('available' in command) || command.available(context)),
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

function ProductToolbar({ context }: { context: ProductCommandContext }) {
  const app = useAppSelector(selectApp)
  return (
    <div className="toolbar" role="toolbar" aria-label="Application controls">
      <button
        className="icon-button mobile-only"
        type="button"
        aria-label="Open navigation"
        onClick={() => context.dispatch(actions.overlaySet('navigation'))}
      >
        <Menu />
      </button>
      <button
        className="icon-button"
        type="button"
        aria-label={`Use ${app.density === 'compact' ? 'comfortable' : 'compact'} density`}
        onClick={() => invokeProductCommand('density.toggle', context)}
      >
        <Rows3 />
      </button>
      <button
        className="icon-button"
        type="button"
        aria-label={`Switch to ${app.layout === 'focus' ? 'workbench' : 'focus'} layout`}
        onClick={() => invokeProductCommand('layout.toggle', context)}
      >
        <PanelLeftClose />
      </button>
      <button
        className="icon-button"
        type="button"
        aria-label={`Use ${app.theme === 'dark' ? 'light' : 'dark'} theme`}
        onClick={() => invokeProductCommand('theme.toggle', context)}
      >
        {app.theme === 'dark' ? <Sun /> : <Moon />}
      </button>
      <button
        className="icon-button"
        type="button"
        aria-label="Open command palette"
        onClick={() => invokeProductCommand('palette.open', context)}
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
  const timelineRef = useRef<HTMLDivElement>(null)
  const [timelineIds, setTimelineIds] = useState<readonly string[]>([])
  const [timelineWindowAvailable, setTimelineWindowAvailable] = useState(false)
  const [windowRequest, setWindowRequest] = useState<{
    anchor: 'first' | 'latest'
    attempt: number
  } | null>(null)
  const updateTimelineIds = useCallback((ids: readonly string[]) => setTimelineIds(ids), [])
  const consumeWindowRequest = useCallback(() => setWindowRequest(null), [])
  const bootstrap = useQuery({
    queryKey: ['production', 'bootstrap'],
    queryFn: ({ signal }) => productTransport.readBootstrap(signal),
    staleTime: Number.POSITIVE_INFINITY,
  })
  const context = useMemo<ProductCommandContext>(
    () => ({
      dispatch,
      getState: store.getState,
      timelineIds,
      timelineWindowAvailable: surface === 'sessions' && timelineWindowAvailable,
      focusTimeline: () => (timelineRef.current ?? primaryRef.current)?.focus(),
      loadTimelineWindow: (anchor) =>
        setWindowRequest((current) => ({ anchor, attempt: (current?.attempt ?? 0) + 1 })),
      navigate: (path) => void navigate({ to: '/$surface', params: { surface: path.slice(1) } }),
    }),
    [dispatch, navigate, surface, timelineIds, timelineWindowAvailable],
  )
  useHotkeys(
    productHotkeyBindings.map((binding) => ({
      hotkey: binding.hotkey,
      callback: () => {
        if (store.getState().app.overlay === null || binding.commandId === 'surface.escape') {
          invokeProductCommand(binding.commandId, context)
        }
      },
    })),
  )
  useHotkeySequences(
    productHotkeySequenceBindings.map((binding) => ({
      sequence: binding.sequence,
      callback: () => {
        if (store.getState().app.overlay === null) {
          invokeProductCommand(binding.commandId, context)
        }
      },
    })),
  )

  useEffect(() => {
    document.documentElement.dataset.theme = app.theme
    document.documentElement.dataset.density = app.density
  }, [app.density, app.theme])

  const copy = surfaceCopy[surface]
  const cacheLabel = productSurfaceCacheLabel(surface)
  const timelineCapability = bootstrap.isPending
    ? 'checking'
    : bootstrap.isSuccess && bootstrap.data.capabilities.bounded_session_timeline
      ? 'available'
      : 'unavailable'

  useEffect(() => {
    const previousTitle = document.title
    document.title = `${copy.title} · Signalbox`
    return () => {
      document.title = previousTitle
    }
  }, [copy.title])

  const content =
    surface === 'attention' ? (
      <AttentionSurface />
    ) : surface === 'sessions' ? (
      <SessionWorkspaceSurface
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
        <ProductNavigation active={surface} />
      </aside>
      <main className="product-main" tabIndex={-1} ref={primaryRef}>
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
          >
            {bootstrap.isSuccess
              ? `${bootstrap.data.contract.name} · ${bootstrap.data.contract.version}`
              : bootstrap.isError
                ? 'Transport unavailable'
                : 'Checking contract…'}
          </span>
          {bootstrap.isError && (
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
          <p>Select an available operational record to inspect its server-provided evidence.</p>
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
          </dl>
        </aside>
      )}
      <CommandPalette context={context} />
      <KeyboardHelp context={context} />
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
              event.preventDefault()
              document.querySelector<HTMLElement>('[aria-label="Open navigation"]')?.focus()
            }}
          >
            <Dialog.Title className="sr-only">Product navigation</Dialog.Title>
            <Dialog.Description id="mobile-navigation-description" className="sr-only">
              Choose a Signalbox surface.
            </Dialog.Description>
            <ProductNavigation
              active={surface}
              onActivate={() => dispatch(actions.overlaySet(null))}
            />
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
    </div>
  )
}
