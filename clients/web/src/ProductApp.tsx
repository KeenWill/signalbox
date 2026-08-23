import * as Dialog from '@radix-ui/react-dialog'
import { useHotkeySequences, useHotkeys } from '@tanstack/react-hotkeys'
import { useQuery } from '@tanstack/react-query'
import { Link, useNavigate } from '@tanstack/react-router'
import { AlertTriangle, Command, Menu, Moon, PanelLeftClose, Rows3, Sun, X } from 'lucide-react'
import { useCallback, useEffect, useMemo, useRef } from 'react'
import {
  type CommandContext,
  commandRegistry,
  globalHotkeyBindings,
  globalHotkeySequenceBindings,
  invokeCommand,
} from './commands'
import {
  type ProductRouteId,
  type ProductSessionState,
  productRoutes,
  productTransport,
} from './product'
import { SessionCatalogSurface } from './SessionCatalogSurface'
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

function ProductNavigation({
  active,
  onNavigate,
}: {
  active: ProductRouteId
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
            onClick={onNavigate}
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
  onCloseAutoFocus,
}: {
  context: CommandContext
  onCloseAutoFocus: (event: Event) => void
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
          onCloseAutoFocus={onCloseAutoFocus}
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
              .filter(
                (command) =>
                  command.id !== 'surface.escape' &&
                  command.id !== 'palette.open' &&
                  command.available(context),
              )
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

export function ProductApp({
  surface,
  search,
}: {
  surface: ProductRouteId
  search: ProductSessionState
}) {
  const dispatch = useAppDispatch()
  const app = useAppSelector(selectApp)
  const navigate = useNavigate()
  const primaryRef = useRef<HTMLElement>(null)
  const sessionOpenedHere = useRef(false)
  const focusAfterBootstrapRetry = useRef(false)
  const focusAfterCommandNavigation = useRef(false)
  const currentSession = useRef(search.session)
  const bootstrap = useQuery({
    queryKey: ['production', 'bootstrap'],
    queryFn: ({ signal }) => productTransport.readBootstrap(signal),
    staleTime: Number.POSITIVE_INFINITY,
  })
  const updateSearch = useCallback(
    (next: ProductSessionState, mode: 'push' | 'close' = 'push') => {
      if (mode === 'close') {
        currentSession.current = next.session
        if (sessionOpenedHere.current) {
          sessionOpenedHere.current = false
          window.history.back()
          return
        }
        void navigate({ to: '/$surface', params: { surface }, search: next, replace: true })
        return
      }
      const previousSession = currentSession.current
      if (!previousSession && next.session) {
        sessionOpenedHere.current = true
      }
      const switchesSelectedSession =
        previousSession !== undefined &&
        next.session !== undefined &&
        next.session !== previousSession
      currentSession.current = next.session
      void navigate({
        to: '/$surface',
        params: { surface },
        search: next,
        replace: switchesSelectedSession,
      })
    },
    [navigate, surface],
  )
  const dismissSession = useCallback(() => {
    updateSearch({ ...search, session: undefined }, 'close')
  }, [search, updateSearch])
  const context = useMemo<CommandContext>(
    () => ({
      dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => {
        if (search.session) dismissSession()
        else primaryRef.current?.focus()
      },
      navigate: (path) => {
        const destination = path.slice(1) as ProductRouteId
        focusAfterCommandNavigation.current = true
        primaryRef.current?.focus()
        void navigate({ to: '/$surface', params: { surface: destination } }).then(() => {
          focusAfterCommandNavigation.current = false
          requestAnimationFrame(() => primaryRef.current?.focus())
        })
      },
    }),
    [dismissSession, dispatch, navigate, search.session],
  )
  useHotkeys(
    globalHotkeyBindings.map((binding) => ({
      hotkey: binding.hotkey,
      callback: (event) => {
        const target = event.target
        const escapeHandledByOverlay =
          binding.commandId === 'surface.escape' &&
          target instanceof Element &&
          target.closest('.product-palette, .mobile-navigation') !== null
        if (store.getState().app.overlay !== null || escapeHandledByOverlay) return
        if (
          binding.commandId === 'surface.escape' &&
          target instanceof HTMLElement &&
          (target.matches('input, textarea, select') || target.isContentEditable)
        ) {
          primaryRef.current?.focus()
          return
        }
        if (
          binding.commandId === 'layout.toggle' &&
          document.activeElement?.closest('.product-navigation-pane')
        ) {
          primaryRef.current?.focus()
        }
        invokeCommand(binding.commandId, context)
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
    document.documentElement.dataset.theme = app.theme
    document.documentElement.dataset.density = app.density
  }, [app.density, app.theme])

  useEffect(() => {
    currentSession.current = search.session
  }, [search.session])

  useEffect(() => {
    if (app.overlay === 'help') dispatch(actions.overlaySet(null))
  }, [app.overlay, dispatch])

  useEffect(() => {
    if (!bootstrap.isSuccess || !focusAfterBootstrapRetry.current) return
    focusAfterBootstrapRetry.current = false
    const frame = requestAnimationFrame(() => primaryRef.current?.focus())
    return () => cancelAnimationFrame(frame)
  }, [bootstrap.isSuccess])

  const copy = surfaceCopy[surface]
  const content =
    surface === 'attention' ? (
      <AttentionSurface />
    ) : surface === 'sessions' && bootstrap.isSuccess ? (
      <SessionCatalogSurface state={search} onStateChange={updateSearch} />
    ) : surface === 'sessions' ? (
      <div className="catalog-notice">
        <p>Sessions are unavailable until the browser contract handshake succeeds.</p>
        {bootstrap.isError && (
          <button
            type="button"
            onClick={() => {
              focusAfterBootstrapRetry.current = true
              void bootstrap.refetch()
            }}
          >
            Retry contract handshake
          </button>
        )}
      </div>
    ) : (
      <DeferredSurface surface={surface} />
    )

  return (
    <div className={`product-shell layout-${app.layout}`}>
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
                ? 'Contract handshake failed'
                : 'Checking contract…'}
          </span>
        </div>
        {content}
      </main>
      {app.layout === 'workbench' && surface !== 'sessions' && (
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
              <dd>Daemon</dd>
            </div>
            <div>
              <dt>Cache</dt>
              <dd>Bounded query</dd>
            </div>
          </dl>
        </aside>
      )}
      <CommandPalette
        context={context}
        onCloseAutoFocus={(event) => {
          if (!focusAfterCommandNavigation.current) return
          event.preventDefault()
        }}
      />
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
              onNavigate={() => dispatch(actions.overlaySet(null))}
            />
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
    </div>
  )
}
