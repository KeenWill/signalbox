import * as Dialog from '@radix-ui/react-dialog'
import { useHotkeySequences, useHotkeys } from '@tanstack/react-hotkeys'
import { useQuery } from '@tanstack/react-query'
import { Link, useNavigate } from '@tanstack/react-router'
import { AlertTriangle, Command, Menu, Moon, PanelLeftClose, Rows3, Sun, X } from 'lucide-react'
import {
  type CSSProperties,
  type MouseEvent,
  type RefObject,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react'
import {
  type ProductRouteId,
  productRoutes,
  productSurfaceStates,
  productTransport,
} from './product'
import {
  invokeProductCommand,
  type ProductCommandContext,
  type ProductCommandId,
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
  context,
  onNavigate,
}: {
  active: ProductRouteId
  context: ProductCommandContext
  onNavigate?: () => void
}) {
  const invokeNavigation = (event: MouseEvent<HTMLAnchorElement>, route: ProductRouteId) => {
    if (
      event.defaultPrevented ||
      event.button !== 0 ||
      event.metaKey ||
      event.ctrlKey ||
      event.shiftKey ||
      event.altKey
    ) {
      return
    }
    event.preventDefault()
    invokeProductCommand(`navigate.${route}` as ProductCommandId, context)
  }
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
            onClick={(event) => invokeNavigation(event, route.id)}
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
        onClick={onNavigate}
      >
        Scenario studio <span aria-hidden="true">↗</span>
      </Link>
    </div>
  )
}

function CommandPalette({
  context,
  openerRef,
  helpOpenerRef,
  fallbackRef,
}: {
  context: ProductCommandContext
  openerRef: RefObject<HTMLElement | null>
  helpOpenerRef: RefObject<HTMLElement | null>
  fallbackRef: RefObject<HTMLElement | null>
}) {
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
          onCloseAutoFocus={(event) => {
            event.preventDefault()
            const opener = openerRef.current
            if (opener?.isConnected && opener.getClientRects().length > 0) opener.focus()
            else fallbackRef.current?.focus()
            openerRef.current = null
          }}
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
                    if (command.id === 'help.open') helpOpenerRef.current = openerRef.current
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

function KeyboardHelp({
  context,
  openerRef,
  fallbackRef,
}: {
  context: ProductCommandContext
  openerRef: RefObject<HTMLElement | null>
  fallbackRef: RefObject<HTMLElement | null>
}) {
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
          className="dialog-content"
          aria-describedby="keyboard-help-description"
          onCloseAutoFocus={(event) => {
            event.preventDefault()
            const opener = openerRef.current
            if (opener?.isConnected && opener.getClientRects().length > 0) opener.focus()
            else fallbackRef.current?.focus()
            openerRef.current = null
          }}
        >
          <div className="dialog-heading">
            <div>
              <Dialog.Title>Keyboard help</Dialog.Title>
              <Dialog.Description id="keyboard-help-description">
                Available workstation commands and their default bindings.
              </Dialog.Description>
            </div>
            <Dialog.Close asChild>
              <button className="icon-button" type="button" aria-label="Close keyboard help">
                <X />
              </button>
            </Dialog.Close>
          </div>
          <dl className="shortcut-list">
            {productCommandRegistry
              .filter(
                (command) =>
                  command.bindings.some((binding) => 'registration' in binding) &&
                  (!('available' in command) || command.available(context)),
              )
              .map((command) => (
                <div key={command.id}>
                  <dt>{command.title}</dt>
                  <dd>
                    {command.bindings
                      .filter((binding) => 'registration' in binding)
                      .map((binding) => (
                        <kbd key={binding.label}>{binding.label}</kbd>
                      ))}
                  </dd>
                </div>
              ))}
          </dl>
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
        onClick={() => invokeProductCommand('navigation.open', context)}
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
  const navigationOpenerRef = useRef<HTMLElement | null>(null)
  const paletteOpenerRef = useRef<HTMLElement | null>(null)
  const helpOpenerRef = useRef<HTMLElement | null>(null)
  const timelineWindowNavigationRef = useRef<(anchor: 'first' | 'latest') => void>(() => {})
  const [timelineIds, setTimelineIds] = useState<readonly string[]>([])
  const [timelineWindowAvailable, setTimelineWindowAvailable] = useState(false)
  const updateTimelineIds = useCallback((ids: readonly string[]) => setTimelineIds(ids), [])
  const focusPrimarySurface = useCallback(() => primaryRef.current?.focus(), [])
  const updateTimelineWindowNavigation = useCallback(
    (navigateTimelineWindow: (anchor: 'first' | 'latest') => void) => {
      timelineWindowNavigationRef.current = navigateTimelineWindow
    },
    [],
  )
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
      focusTimeline: () => {
        const selected = store.getState().app.selectedTimeline ?? timelineIds[0]
        const control = selected
          ? document.querySelector<HTMLElement>(`[data-timeline-id="${selected}"]`)
          : null
        ;(control ?? primaryRef.current)?.focus()
      },
      navigate: (path) => {
        void navigate({ to: '/$surface', params: { surface: path.slice(1) } }).then(() =>
          primaryRef.current?.focus(),
        )
      },
      navigateTimelineWindow: (anchor) => timelineWindowNavigationRef.current(anchor),
      openNavigation: () => {
        const active = document.activeElement
        navigationOpenerRef.current =
          active instanceof HTMLElement && active !== document.body ? active : null
        dispatch(actions.overlaySet('navigation'))
      },
      openPalette: () => {
        const active = document.activeElement
        paletteOpenerRef.current =
          active instanceof HTMLElement && active !== document.body ? active : null
        dispatch(actions.overlaySet('palette'))
      },
      prepareFocusLayout: () => {
        const active = document.activeElement
        if (
          active instanceof HTMLElement &&
          (active.closest('.product-navigation-pane') || active.closest('.product-inspector'))
        ) {
          primaryRef.current?.focus()
        }
      },
      timelineWindowAvailable: surface === 'sessions' && timelineWindowAvailable,
    }),
    [dispatch, navigate, surface, timelineIds, timelineWindowAvailable],
  )
  useHotkeys(
    productHotkeyBindings.map((binding) => ({
      hotkey: binding.hotkey,
      callback: () => {
        if (binding.commandId === 'surface.escape' || store.getState().app.overlay === null) {
          if (binding.commandId === 'help.open') {
            const active = document.activeElement
            helpOpenerRef.current =
              active instanceof HTMLElement && active !== document.body ? active : null
          }
          invokeProductCommand(binding.commandId, context)
        }
      },
    })),
  )
  useHotkeySequences(
    productHotkeySequenceBindings.map((binding) => ({
      sequence: [...binding.sequence],
      callback: () => {
        if (store.getState().app.overlay === null) {
          invokeProductCommand(binding.commandId, context)
        }
      },
    })),
  )

  useEffect(() => {
    if (app.selectedTimeline === null) return
    document.querySelector<HTMLElement>(`[data-timeline-id="${app.selectedTimeline}"]`)?.focus()
  }, [app.selectedTimeline])

  useEffect(() => {
    document.documentElement.dataset.theme = app.theme
    document.documentElement.dataset.density = app.density
  }, [app.density, app.theme])

  const copy = surfaceCopy[surface]
  const surfaceUnavailable =
    productSurfaceStates[surface].kind === 'committed-unimplemented' ||
    (surface === 'sessions' && bootstrap.data?.capabilities.bounded_session_timeline !== true)
  const content =
    surface === 'attention' ? (
      <AttentionSurface />
    ) : surface === 'sessions' ? (
      <SessionWorkspaceSurface
        bootstrap={bootstrap.data}
        onTimelineIds={updateTimelineIds}
        onTimelineWindowNavigation={updateTimelineWindowNavigation}
        onTimelineWindowAvailability={setTimelineWindowAvailable}
        onTimelineWindowCommand={(command) => invokeProductCommand(command, context)}
        onEmptyTimelineFocus={focusPrimarySurface}
      />
    ) : surface === 'settings' ? (
      <SettingsSurface context={context} />
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
            <button type="button" onClick={() => void bootstrap.refetch()}>
              Retry bootstrap
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
              <dd>
                {surface === 'settings' ? 'Browser' : surfaceUnavailable ? 'Unavailable' : 'Daemon'}
              </dd>
            </div>
            <div>
              <dt>Cache</dt>
              <dd>
                {surface === 'settings'
                  ? 'Local settings'
                  : surfaceUnavailable
                    ? 'No operational query'
                    : 'Bounded query'}
              </dd>
            </div>
          </dl>
        </aside>
      )}
      <CommandPalette
        context={context}
        openerRef={paletteOpenerRef}
        helpOpenerRef={helpOpenerRef}
        fallbackRef={primaryRef}
      />
      <KeyboardHelp context={context} openerRef={helpOpenerRef} fallbackRef={primaryRef} />
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
              const opener = navigationOpenerRef.current
              if (opener?.isConnected && opener.getClientRects().length > 0) opener.focus()
              else primaryRef.current?.focus()
              navigationOpenerRef.current = null
            }}
          >
            <Dialog.Title className="sr-only">Product navigation</Dialog.Title>
            <Dialog.Description id="mobile-navigation-description" className="sr-only">
              Choose a Signalbox surface.
            </Dialog.Description>
            <ProductNavigation
              active={surface}
              context={context}
              onNavigate={() => dispatch(actions.overlaySet(null))}
            />
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
    </div>
  )
}
