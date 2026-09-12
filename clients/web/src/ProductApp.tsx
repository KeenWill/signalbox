import * as Dialog from '@radix-ui/react-dialog'
import { useHotkeySequences, useHotkeys } from '@tanstack/react-hotkeys'
import { useQuery } from '@tanstack/react-query'
import { Link, useLocation, useNavigate } from '@tanstack/react-router'
import {
  Activity,
  AlertTriangle,
  Bell,
  ChartNoAxesCombined,
  Command,
  Download,
  FileSearch,
  GitPullRequest,
  Home,
  Menu,
  MessagesSquare,
  Moon,
  PanelLeftClose,
  PanelLeftOpen,
  Search,
  Settings,
  Sun,
  X,
} from 'lucide-react'
import {
  type CSSProperties,
  createContext,
  type ReactNode,
  type RefObject,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react'
import { createPortal } from 'react-dom'
import {
  ArtifactInspector,
  artifactResolutionId,
  emptyArtifactInspectorState,
} from './ArtifactInspector'
import { AttentionSurface } from './AttentionSurface'
import type { CommandContext } from './commands'
import { HttpImportApi } from './imports/api'
import { ImportsWorkspace } from './imports/ImportsWorkspace'
import { loadRetainedCommand } from './imports/retainedCommand'
import {
  ProductContractError,
  type ProductRouteId,
  type ProductRouteState,
  type ProductSearchState,
  type ProductSessionState,
  ProductTransportError,
  productRoutes,
  productSurfaceStates,
  productTransport,
  readProductSessionState,
} from './product'
import {
  invokeProductCommand,
  type ProductCommandContext,
  type ProductCommandId,
  productCommandAvailable,
  productCommandRegistry,
  productHotkeyBindings,
  productHotkeySequenceBindings,
} from './productCommands'
import { SearchSurface } from './SearchSurface'
import { SessionCatalogSurface } from './SessionCatalogSurface'
import { SessionWorkspaceSurface } from './SessionWorkspaceSurface'
import { SettingsSurface } from './SettingsSurface'
import { hasValidSessionTimelineContract } from './session-timeline/model'
import { actions, selectApp, store, useAppDispatch, useAppSelector } from './state'

declare module '@tanstack/react-router' {
  interface HistoryState {
    catalogSessionOpenedHere?: boolean
  }
}

const isEditableTarget = (target: EventTarget | null) => {
  if (!(target instanceof HTMLElement)) return false
  return (
    target.isContentEditable ||
    target instanceof HTMLInputElement ||
    target instanceof HTMLTextAreaElement ||
    target instanceof HTMLSelectElement
  )
}

const productNavigationCommandIds: Record<ProductRouteId, ProductCommandId> = {
  attention: 'navigate.attention',
  sessions: 'navigate.sessions',
  search: 'navigate.search',
  runners: 'navigate.runners',
  reviews: 'navigate.reviews',
  imports: 'navigate.imports',
  usage: 'navigate.usage',
  settings: 'navigate.settings',
}

const productNavigationIcons = {
  attention: Bell,
  sessions: MessagesSquare,
  search: Search,
  runners: Activity,
  reviews: GitPullRequest,
  imports: Download,
  usage: ChartNoAxesCombined,
  settings: Settings,
}

export function ProductNavigation({
  active,
  context,
  onActivate,
  collapsed,
}: {
  collapsed?: boolean
  active: ProductRouteId
  context: ProductCommandContext
  onActivate?: () => void
}) {
  const scenarioDisabled = !productCommandAvailable('navigate.scenario', context)
  return (
    <div className={`product-navigation ${collapsed ? 'navigation-collapsed' : ''}`}>
      <div className="product-brand-row">
        <Link
          className="brand product-brand"
          to="/$surface"
          params={{ surface: 'attention' }}
          aria-label="Signalbox home"
          aria-disabled={context.navigationLocked || undefined}
          tabIndex={context.navigationLocked ? -1 : undefined}
          onClick={(event) => {
            if (context.navigationLocked) {
              event.preventDefault()
              return
            }
            if (
              event.button !== 0 ||
              event.metaKey ||
              event.ctrlKey ||
              event.shiftKey ||
              event.altKey
            ) {
              return
            }
            event.preventDefault()
            onActivate?.()
            invokeProductCommand('navigate.attention', context)
          }}
        >
          {collapsed ? <Home aria-hidden="true" /> : <strong>Signalbox</strong>}
        </Link>
        {collapsed !== undefined && (
          <button
            type="button"
            className="icon-button sidebar-toggle"
            aria-label={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
            aria-expanded={!collapsed}
            onClick={() => invokeProductCommand('navigation.toggle', context)}
          >
            {collapsed ? <PanelLeftOpen /> : <PanelLeftClose />}
          </button>
        )}
      </div>
      <nav aria-label="Product">
        {productRoutes.map((route) => {
          const disabled = !productCommandAvailable(productNavigationCommandIds[route.id], context)
          const Icon = productNavigationIcons[route.id]
          return (
            <Link
              key={route.id}
              aria-label={route.label}
              title={collapsed ? route.label : undefined}
              to="/$surface"
              params={{ surface: route.id }}
              className={active === route.id ? 'product-link active' : 'product-link'}
              aria-current={active === route.id ? 'page' : undefined}
              aria-disabled={disabled || undefined}
              tabIndex={disabled ? -1 : undefined}
              onClick={(event) => {
                if (disabled) {
                  event.preventDefault()
                  return
                }
                if (
                  event.button !== 0 ||
                  event.metaKey ||
                  event.ctrlKey ||
                  event.shiftKey ||
                  event.altKey
                ) {
                  return
                }
                event.preventDefault()
                onActivate?.()
                invokeProductCommand(productNavigationCommandIds[route.id], context)
              }}
            >
              <Icon aria-hidden="true" />
              {!collapsed && <span>{route.label}</span>}
            </Link>
          )
        })}
      </nav>
      <Link
        className="scenario-entry"
        to="/scenario/$scenarioId"
        params={{ scenarioId: 'streaming' }}
        aria-disabled={scenarioDisabled || undefined}
        tabIndex={scenarioDisabled ? -1 : undefined}
        onClick={(event) => {
          if (scenarioDisabled) {
            event.preventDefault()
            return
          }
          onActivate?.()
        }}
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
  const focusTimelineAfterClose = useRef(false)
  const focusSearchAfterClose = useRef(false)
  const openArtifactAfterClose = useRef(false)
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
          onEscapeKeyDown={(event) => event.stopPropagation()}
          onCloseAutoFocus={(event) => {
            if (openArtifactAfterClose.current) {
              event.preventDefault()
              openArtifactAfterClose.current = false
              invokeProductCommand('artifact.open', context)
              return
            }
            if (focusSearchAfterClose.current) {
              event.preventDefault()
              focusSearchAfterClose.current = false
              context.focusSearch?.()
              return
            }
            if (focusTimelineAfterClose.current) {
              event.preventDefault()
              focusTimelineAfterClose.current = false
              context.focusTimeline()
              return
            }
            // Hand the palette's keystroke back to the control it was invoked from, unless another
            // overlay has already taken over the surface.
            const opener = openerRef.current
            if (context.getState().app.overlay !== null) return
            event.preventDefault()
            if (opener?.isConnected && opener.getClientRects().length > 0) opener.focus()
            else fallbackRef.current?.focus()
          }}
        >
          <div className="dialog-heading">
            <div>
              <Dialog.Title>Command palette</Dialog.Title>
              <Dialog.Description id="product-palette-description" className="sr-only">
                Choose a command.
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
                  command.id !== 'palette.open' &&
                  (!('available' in command) || command.available(context)),
              )
              .map((command) => (
                <button
                  key={command.id}
                  type="button"
                  onClick={() => {
                    openArtifactAfterClose.current = command.id === 'artifact.open'
                    focusSearchAfterClose.current = command.id === 'search.focus'
                    focusTimelineAfterClose.current =
                      command.id.startsWith('selection.') &&
                      productCommandAvailable(command.id, context)
                    if (command.id === 'help.open') helpOpenerRef.current = openerRef.current
                    invokeProductCommand('surface.escape', context)
                    if (!openArtifactAfterClose.current) invokeProductCommand(command.id, context)
                  }}
                >
                  <span>
                    <strong>{command.title}</strong>
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

function OpenSessionDialog({
  context,
  openerRef,
  fallbackRef,
}: {
  context: ProductCommandContext
  openerRef: RefObject<HTMLElement | null>
  fallbackRef: RefObject<HTMLElement | null>
}) {
  const open = useAppSelector((state) => state.app.overlay === 'session-entry')
  const [sessionId, setSessionId] = useState('')
  const entryRef = useRef<HTMLInputElement>(null)
  const submitted = useRef(false)
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
          className="dialog-content open-session-dialog"
          aria-describedby="open-session-description"
          onOpenAutoFocus={(event) => {
            event.preventDefault()
            entryRef.current?.focus()
          }}
          onEscapeKeyDown={(event) => event.stopPropagation()}
          onCloseAutoFocus={(event) => {
            event.preventDefault()
            setSessionId('')
            const opener = openerRef.current
            if (!submitted.current && opener?.isConnected && opener.getClientRects().length > 0) {
              opener.focus()
            } else {
              fallbackRef.current?.focus()
            }
            submitted.current = false
          }}
        >
          <div className="dialog-heading">
            <Dialog.Title>Open session by id</Dialog.Title>
            <Dialog.Close asChild>
              <button className="icon-button" type="button" aria-label="Close open session">
                <X />
              </button>
            </Dialog.Close>
          </div>
          <Dialog.Description id="open-session-description">
            Paste a session identifier.
          </Dialog.Description>
          <form
            className="open-session-command"
            onSubmit={(event) => {
              event.preventDefault()
              if (context.navigationLocked) return
              submitted.current = true
              invokeProductCommand('surface.escape', context)
              invokeProductCommand('session.open', {
                ...context,
                sessionId: sessionId.trim().toLowerCase(),
              })
            }}
          >
            <label htmlFor="palette-session-id">Session ID</label>
            <input
              id="palette-session-id"
              ref={entryRef}
              value={sessionId}
              onChange={(event) => setSessionId(event.currentTarget.value.trim())}
              required
              pattern="[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
              autoComplete="off"
            />
            <button type="submit" disabled={context.navigationLocked}>
              Open session
            </button>
          </form>
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
          className="dialog-content product-palette"
          aria-describedby="keyboard-help-description"
          onEscapeKeyDown={(event) => event.stopPropagation()}
          onCloseAutoFocus={(event) => {
            event.preventDefault()
            const opener = openerRef.current
            openerRef.current = null
            if (context.getState().app.overlay !== null) return
            if (opener?.isConnected && opener.getClientRects().length > 0) opener.focus()
            else fallbackRef.current?.focus()
          }}
        >
          <div className="dialog-heading">
            <div>
              <Dialog.Title>Keyboard help</Dialog.Title>
              <Dialog.Description id="keyboard-help-description" className="sr-only">
                Keyboard shortcuts.
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
        <h2 id={`${surface}-unavailable-heading`}>
          {productRoutes.find((route) => route.id === surface)?.label} unavailable
        </h2>
      </div>
    </section>
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
  artifactAvailable,
  artifactButtonRef,
  context,
  onOpenPalette,
}: {
  artifactAvailable: boolean
  artifactButtonRef: RefObject<HTMLButtonElement | null>
  context: ProductCommandContext
  onOpenPalette: (opener: HTMLElement) => void
}) {
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
        ref={artifactButtonRef}
        className="icon-button"
        type="button"
        aria-label="Open artifact inspector"
        disabled={!artifactAvailable}
        onClick={() => invokeProductCommand('artifact.open', context)}
      >
        <FileSearch />
      </button>
      <button
        className="layout-button"
        type="button"
        aria-label={`Switch to ${app.layout === 'focus' ? 'workbench' : 'focus'} layout`}
        onClick={() => invokeProductCommand('layout.toggle', context)}
      >
        <PanelLeftClose />
        <span>{app.layout === 'focus' ? 'Workbench' : 'Focus'}</span>
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
        onClick={(event) => {
          onOpenPalette(event.currentTarget)
          invokeProductCommand('palette.open', context)
        }}
      >
        <Command />
      </button>
    </div>
  )
}

// Must stay identical to the `.product-inspector { display: none }` breakpoint in app.css:
// a wider composition threshold than the visibility threshold would mount the side pane into a
// hidden aside and focus a Digest input nobody can see.
const INSPECTOR_SHEET_MEDIA = '(max-width: 1260px)'

function useMediaQuery(media: string): boolean {
  const [narrow, setNarrow] = useState(() => window.matchMedia(media).matches)
  useEffect(() => {
    const query = window.matchMedia(media)
    const update = () => setNarrow(query.matches)
    query.addEventListener('change', update)
    return () => query.removeEventListener('change', update)
  }, [media])
  return narrow
}

const SurfaceHeaderTarget = createContext<HTMLDivElement | null>(null)

export function SurfaceHeaderActions({ headerActions }: { headerActions?: ReactNode }) {
  const target = useContext(SurfaceHeaderTarget)
  return target && headerActions ? createPortal(headerActions, target) : null
}

export function ProductApp({
  surface,
  search,
}: {
  surface: ProductRouteId
  search: ProductRouteState
}) {
  const [headerTarget, setHeaderTarget] = useState<HTMLDivElement | null>(null)
  const dispatch = useAppDispatch()
  const app = useAppSelector(selectApp)
  const navigate = useNavigate()
  const mainRef = useRef<HTMLElement>(null)
  const timelineRef = useRef<HTMLDivElement>(null)
  const paletteOpenerRef = useRef<HTMLElement | null>(null)
  const helpOpenerRef = useRef<HTMLElement | null>(null)
  const navigationOpenerRef = useRef<HTMLElement | null>(null)
  const artifactButtonRef = useRef<HTMLButtonElement>(null)
  const artifactDigestRef = useRef<HTMLInputElement>(null)
  const sessionState = useMemo(() => readProductSessionState({ ...search }), [search])
  const catalogSessionOpenedHere = useLocation({
    select: (location) => location.state.catalogSessionOpenedHere === true,
  })
  const currentCatalogSession = useRef(sessionState.session)
  useEffect(() => {
    if (currentCatalogSession.current !== sessionState.session || surface !== 'sessions') {
      currentCatalogSession.current = sessionState.session
    }
  }, [sessionState.session, surface])
  const artifactSideWasOpen = useRef(false)
  const inspectorWasInSheet = useRef(false)
  const surfaceEscapeRef = useRef<(() => boolean) | null>(null)
  const registerSurfaceEscape = useCallback((handler: (() => boolean) | null) => {
    surfaceEscapeRef.current = handler
  }, [])
  const [artifactOpen, setArtifactOpen] = useState(false)
  const [artifactInspectorState, setArtifactInspectorState] = useState(emptyArtifactInspectorState)
  const artifactRequest = artifactInspectorState.request
  useEffect(() => {
    if (artifactRequest === null) return
    return () => {
      dispatch(actions.artifactOriginalReleased(artifactResolutionId(artifactRequest)))
    }
  }, [artifactRequest, dispatch])
  const narrowInspector = useMediaQuery(INSPECTOR_SHEET_MEDIA)
  const narrowNavigation = useMediaQuery('(max-width: 760px)')
  const [timelineIds, setTimelineIds] = useState<readonly string[]>([])
  const [timelineWindowAvailable, setTimelineWindowAvailable] = useState(false)
  const [windowRequest, setWindowRequest] = useState<{
    anchor: 'first' | 'latest'
    attempt: number
  } | null>(null)
  const [importsCommandContext, setImportsCommandContext] = useState<CommandContext | null>(null)
  const [retainedNavigationLock, setNavigationDisabled] = useState(
    () => loadRetainedCommand('production') !== null,
  )
  const navigationDisabled = surface === 'imports' && retainedNavigationLock
  const updateImportsCommandContext = useCallback(
    (next: CommandContext | null) => setImportsCommandContext(next),
    [],
  )
  const updateTimelineIds = useCallback((ids: readonly string[]) => setTimelineIds(ids), [])
  const consumeWindowRequest = useCallback(() => setWindowRequest(null), [])
  const [catalogLifecycleFilter, setCatalogLifecycleFilter] = useState('all')
  const [catalogPageOrder, setCatalogPageOrder] = useState('activity')
  const catalogReturnSessionId = useRef<string | undefined>(undefined)
  const consumeCatalogReturnFocus = useCallback(() => {
    catalogReturnSessionId.current = undefined
  }, [])
  const updateSessionSearch = useCallback(
    (next: ProductSessionState, mode: 'push' | 'close' | 'replace' = 'push') => {
      if (mode === 'push' && next.workspace && next.session)
        catalogReturnSessionId.current = next.session
      if (mode === 'close') {
        currentCatalogSession.current = next.session
        if (catalogSessionOpenedHere) {
          window.history.back()
          return
        }
        void navigate({ to: '/$surface', params: { surface }, search: next, replace: true })
        return
      }
      const previousSession = currentCatalogSession.current
      const switchesSelectedSession =
        previousSession !== undefined &&
        next.session !== undefined &&
        next.session !== previousSession
      currentCatalogSession.current = next.session
      void navigate({
        to: '/$surface',
        params: { surface },
        search: next,
        replace: mode === 'replace' || switchesSelectedSession,
        state: {
          catalogSessionOpenedHere:
            next.session !== undefined &&
            (catalogSessionOpenedHere || (mode === 'push' && previousSession === undefined)),
        },
      })
    },
    [catalogSessionOpenedHere, navigate, surface],
  )
  const bootstrap = useQuery({
    queryKey: ['production', 'bootstrap'],
    queryFn: ({ signal }) => productTransport.readBootstrap(signal),
    staleTime: Number.POSITIVE_INFINITY,
    enabled: surface !== 'settings',
  })
  const artifactAvailable = bootstrap.data?.capabilities.immutable_blob_content === true
  const bootstrapFailure = bootstrap.error
    ? bootstrap.error instanceof ProductTransportError
      ? 'Daemon unreachable'
      : bootstrap.error instanceof ProductContractError
        ? 'Unexpected daemon response'
        : 'Daemon unavailable'
    : null
  const inspectorInSheet = app.layout === 'focus' || narrowInspector
  // Imports reads and continuation mutations are admitted by the same bootstrap the shell validated.
  const revalidateBootstrap = bootstrap.refetch
  const productImportApi = useMemo(
    () =>
      bootstrap.data === undefined
        ? null
        : HttpImportApi.withAdmittedBootstrap(bootstrap.data, bootstrap.dataUpdatedAt, async () => {
            await revalidateBootstrap({ throwOnError: true })
          }),
    [bootstrap.data, bootstrap.dataUpdatedAt, revalidateBootstrap],
  )
  const context = useMemo<ProductCommandContext>(() => {
    // `productCommandRegistry` already carries the `imports.*` family behind `available()` gates;
    // publishing the mounted surface's context is what makes those commands live.
    const surfaceContext = surface === 'imports' ? importsCommandContext : null
    return {
      ...surfaceContext,
      dispatch,
      getState: store.getState,
      timelineIds: surfaceContext === null ? timelineIds : surfaceContext.timelineIds,
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      timelineWindowAvailable: surface === 'sessions' && timelineWindowAvailable,
      searchAvailable:
        surface === 'search' &&
        bootstrap.isSuccess &&
        bootstrap.data.capabilities.bounded_lexical_search === true,
      focusSearch: () => document.getElementById('product-search-input')?.focus(),
      configuresTranscriptDetail: surface === 'settings',
      focusTimeline:
        surfaceContext?.focusTimeline ??
        (() => {
          if (timelineRef.current !== null) {
            timelineRef.current.focus()
            return
          }
          // A surface with no timeline still has to release an editing control on unwind, but
          // Escape with nothing to unwind must leave focus exactly where it is.
          if (isEditableTarget(document.activeElement)) mainRef.current?.focus()
        }),
      unwindSurface: () => {
        if (surface === 'sessions' && (sessionState.workspace || sessionState.session)) {
          updateSessionSearch(
            { ...sessionState, workspace: undefined, session: undefined },
            'close',
          )
          return true
        }
        return surfaceEscapeRef.current?.() ?? false
      },
      openArtifactInspector: artifactAvailable ? () => setArtifactOpen(true) : undefined,
      loadTimelineWindow:
        sessionState.workspace || sessionState.session
          ? (anchor) =>
              setWindowRequest((current) => ({ anchor, attempt: (current?.attempt ?? 0) + 1 }))
          : undefined,
      openSession: (sessionId) => {
        void navigate({
          to: '/$surface',
          params: { surface: 'sessions' },
          search: { session: sessionId, workspace: true },
        })
      },
      sidebarAvailable: !narrowNavigation && app.layout === 'workbench',
      navigationLocked: navigationDisabled,
      navigate: (path) => {
        if (path === '/scenario/streaming') {
          void navigate({ to: '/scenario/$scenarioId', params: { scenarioId: 'streaming' } })
          return
        }
        void navigate({ to: '/$surface', params: { surface: path.slice(1) } }).then(() => {
          requestAnimationFrame(() => mainRef.current?.focus())
        })
      },
      openNavigation: () => {
        const activeElement = document.activeElement
        const opener =
          activeElement instanceof HTMLElement && activeElement.closest('[role="dialog"]')
            ? paletteOpenerRef.current
            : activeElement instanceof HTMLElement
              ? activeElement
              : null
        navigationOpenerRef.current = opener?.isConnected ? opener : null
        dispatch(actions.overlaySet('navigation'))
      },
    }
  }, [
    app.layout,
    narrowNavigation,
    artifactAvailable,
    bootstrap.data,
    bootstrap.isSuccess,
    dispatch,
    importsCommandContext,
    navigate,
    navigationDisabled,
    sessionState,
    surface,
    timelineIds,
    timelineWindowAvailable,
    updateSessionSearch,
  ])
  useEffect(() => {
    void surface
    void sessionState.session
    void sessionState.workspace
    if (document.activeElement === document.body) mainRef.current?.focus()
  }, [surface, sessionState.session, sessionState.workspace])
  const artifactSheetOwnsFocus = artifactOpen && inspectorInSheet
  useHotkeys(
    productHotkeyBindings
      .filter((binding) => productCommandAvailable(binding.commandId, context))
      .filter(
        (binding) =>
          !binding.commandId.startsWith('imports.') ||
          (app.overlay === null && !navigationDisabled),
      )
      .map((binding) => ({
        hotkey: binding.hotkey,
        // Product surfaces own text fields, so the palette binding must never steal a keystroke the
        // field is editing.
        options: binding.commandId === 'palette.open' ? { ignoreInputs: true } : undefined,
        callback: (event) => {
          if (artifactSheetOwnsFocus) return
          const searchFormOwnsTarget =
            surface === 'search' &&
            event.target instanceof HTMLElement &&
            event.target.closest('.search-form') !== null
          if (
            (binding.commandId === 'palette.open' ||
              (binding.commandId === 'surface.escape' && !searchFormOwnsTarget)) &&
            isEditableTarget(event.target)
          ) {
            return
          }
          if (store.getState().app.overlay === null || binding.commandId === 'surface.escape') {
            if (binding.commandId === 'help.open') {
              const activeElement = document.activeElement
              helpOpenerRef.current = activeElement instanceof HTMLElement ? activeElement : null
            }
            if (binding.commandId === 'palette.open') {
              const activeElement = document.activeElement
              paletteOpenerRef.current = activeElement instanceof HTMLElement ? activeElement : null
            }
            if (
              binding.commandId.startsWith('selection.') &&
              productCommandAvailable(binding.commandId, context)
            ) {
              context.focusTimeline()
            }
            if (binding.commandId === 'layout.toggle' && app.layout === 'workbench') {
              // Focus leaves the navigation pane before the focus layout hides it.
              mainRef.current?.focus()
            }
            invokeProductCommand(binding.commandId, context)
          }
        },
      })),
  )
  useHotkeySequences(
    productHotkeySequenceBindings
      .filter((binding) => productCommandAvailable(binding.commandId, context))
      .filter(
        (binding) =>
          !binding.commandId.startsWith('imports.') ||
          (app.overlay === null && !navigationDisabled),
      )
      .map((binding) => ({
        sequence: binding.sequence,
        callback: (event) => {
          if (artifactSheetOwnsFocus) return
          if (isEditableTarget(event.target)) return
          if (store.getState().app.overlay === null) {
            if (
              binding.commandId.startsWith('selection.') &&
              productCommandAvailable(binding.commandId, context)
            ) {
              context.focusTimeline()
            }
            invokeProductCommand(binding.commandId, context)
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
    const returnedToSidePane = artifactOpen && inspectorWasInSheet.current && !inspectorInSheet
    if (artifactOpen && !inspectorInSheet && (!artifactSideWasOpen.current || returnedToSidePane)) {
      artifactSideWasOpen.current = true
      artifactDigestRef.current?.focus()
    } else if (!artifactOpen) {
      artifactSideWasOpen.current = false
    }
    inspectorWasInSheet.current = inspectorInSheet
  }, [artifactOpen, inspectorInSheet])

  const closeArtifactInspector = useCallback(() => {
    artifactSideWasOpen.current = false
    setArtifactOpen(false)
    requestAnimationFrame(() => artifactButtonRef.current?.focus())
  }, [])

  useEffect(() => {
    if (!artifactOpen || inspectorInSheet) return undefined
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key !== 'Escape' || app.overlay !== null) return
      const target = event.target
      if (
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement ||
        (target instanceof HTMLElement && target.isContentEditable)
      ) {
        return
      }
      event.preventDefault()
      event.stopPropagation()
      closeArtifactInspector()
    }
    window.addEventListener('keydown', closeOnEscape, true)
    return () => window.removeEventListener('keydown', closeOnEscape, true)
  }, [app.overlay, artifactOpen, closeArtifactInspector, inspectorInSheet])

  const title = productRoutes.find((route) => route.id === surface)?.label ?? surface
  const timelineCapability = bootstrap.isPending
    ? 'checking'
    : bootstrap.isSuccess && hasValidSessionTimelineContract(bootstrap.data)
      ? 'available'
      : 'unavailable'

  useEffect(() => {
    const previousTitle = document.title
    document.title = `${title} · Signalbox`
    return () => {
      document.title = previousTitle
    }
  }, [title])

  const updateSearch = (next: ProductSearchState) =>
    void navigate({ to: '/$surface', params: { surface }, search: next })

  const content =
    surface === 'attention' && bootstrap.isSuccess ? (
      <AttentionSurface registerEscapeHandler={registerSurfaceEscape} />
    ) : surface === 'attention' ? (
      <div className="surface-body">
        <section className="surface-empty" role={bootstrap.isError ? 'alert' : 'status'}>
          <div>
            <h2>{bootstrap.isError ? 'Attention unavailable' : 'Loading Attention…'}</h2>
          </div>
        </section>
      </div>
    ) : surface === 'sessions' &&
      bootstrap.isSuccess &&
      (sessionState.workspace || sessionState.session) ? (
      <SessionWorkspaceSurface
        key={sessionState.session ?? 'unselected'}
        onSessionOpen={(session) =>
          updateSessionSearch({ ...sessionState, session, workspace: true }, 'replace')
        }
        focusEntry={sessionState.session === undefined}
        onReturnToCatalog={() => context.unwindSurface?.()}
        initialSessionId={sessionState.session}
        onTimelineIds={updateTimelineIds}
        onTimelineWindowAvailable={setTimelineWindowAvailable}
        onWindowRequestConsumed={consumeWindowRequest}
        timelineCapability={timelineCapability}
        transcriptAvailable={bootstrap.data.capabilities.bounded_session_timeline_detail}
        transcriptLimits={bootstrap.data.limits}
        timelineRef={timelineRef}
        windowRequest={windowRequest}
      />
    ) : surface === 'sessions' && bootstrap.isSuccess ? (
      <SessionCatalogSurface
        returnSessionId={catalogReturnSessionId.current}
        onReturnFocusConsumed={consumeCatalogReturnFocus}
        lifecycleFilter={catalogLifecycleFilter}
        pageOrder={catalogPageOrder}
        onLifecycleFilterChange={setCatalogLifecycleFilter}
        onPageOrderChange={setCatalogPageOrder}
        state={sessionState}
        onStateChange={updateSessionSearch}
        onTimelineIds={setTimelineIds}
      />
    ) : surface === 'sessions' ? (
      <div className="catalog-notice">
        <p>Sessions unavailable</p>
      </div>
    ) : surface === 'search' && bootstrap.isError ? (
      <div className="surface-body">
        <section className="surface-empty" role="alert">
          <AlertTriangle aria-hidden="true" />
          <div>
            <h2>Search unavailable</h2>
          </div>
        </section>
      </div>
    ) : surface === 'search' && bootstrap.data === undefined ? (
      <div className="surface-body">
        <p className="search-notice">Loading search…</p>
      </div>
    ) : surface === 'search' &&
      (bootstrap.data?.capabilities.bounded_json === false ||
        bootstrap.data?.capabilities.bounded_lexical_search === false) ? (
      <div className="surface-body">
        <section className="surface-empty" aria-labelledby="search-unavailable-heading">
          <AlertTriangle aria-hidden="true" />
          <div>
            <h2 id="search-unavailable-heading">Search unavailable</h2>
          </div>
        </section>
      </div>
    ) : surface === 'search' ? (
      <SearchSurface bootstrap={bootstrap.data} state={search} onStateChange={updateSearch} />
    ) : surface === 'settings' ? (
      <SettingsSurface context={context} />
    ) : surface === 'imports' && bootstrap.isSuccess && productImportApi !== null ? (
      <ImportsWorkspace
        api={productImportApi}
        scenario={false}
        presentation="product"
        onCommandContext={updateImportsCommandContext}
        onNavigationDisabledChange={setNavigationDisabled}
      />
    ) : surface === 'imports' ? (
      <div className="surface-body">
        <section className="surface-empty" aria-labelledby="imports-unavailable-heading">
          <AlertTriangle aria-hidden="true" />
          <div>
            <h2 id="imports-unavailable-heading">Imports unavailable</h2>
          </div>
        </section>
      </div>
    ) : surface === 'reviews' ? (
      <DeferredSurface surface="reviews" />
    ) : (
      <DeferredSurface surface={surface} />
    )

  const shellStyle = {
    '--product-navigation-width': app.navigationCollapsed
      ? '56px'
      : `${app.paneSizes.navigation}px`,
    '--product-inspector-width': `${app.paneSizes.inspector}px`,
  } as CSSProperties

  return (
    <div
      className={`product-shell layout-${app.layout} ${artifactOpen && !inspectorInSheet ? 'has-artifact-inspector' : ''}`}
      style={shellStyle}
    >
      <aside className="product-navigation-pane">
        <ProductNavigation active={surface} context={context} collapsed={app.navigationCollapsed} />
      </aside>
      <main className={`product-main product-main-${surface}`} ref={mainRef} tabIndex={-1}>
        <header className="product-header">
          <h1>{title}</h1>
          <div className="product-header-actions">
            <div className="surface-header-actions" ref={setHeaderTarget} />
            {surface !== 'settings' && !bootstrap.isSuccess && (
              <div className="product-connection">
                <span
                  className={`contract-state ${bootstrap.isError ? 'failed' : ''}`}
                  role="status"
                  aria-live="polite"
                  aria-atomic="true"
                  tabIndex={-1}
                >
                  {bootstrap.isError ? bootstrapFailure : 'Connecting…'}
                </span>
                {bootstrap.isError && (
                  <button
                    type="button"
                    className="bootstrap-retry"
                    onClick={(event) => {
                      const opener = event.currentTarget
                      let restoreFocus = document.activeElement === opener
                      const recordBlur = () => {
                        queueMicrotask(() => {
                          if (opener.isConnected) restoreFocus = false
                        })
                      }
                      const recordPointerMove = () => {
                        restoreFocus = false
                      }
                      opener.addEventListener('blur', recordBlur)
                      document.addEventListener('pointerdown', recordPointerMove)
                      void bootstrap.refetch().then((result) => {
                        opener.removeEventListener('blur', recordBlur)
                        document.removeEventListener('pointerdown', recordPointerMove)
                        if (result.isSuccess && restoreFocus) {
                          requestAnimationFrame(() => {
                            if (
                              document.activeElement === opener ||
                              (!opener.isConnected && document.activeElement === document.body)
                            )
                              mainRef.current?.focus()
                          })
                        }
                      })
                    }}
                  >
                    Retry connection
                  </button>
                )}
              </div>
            )}
            <ProductToolbar
              artifactAvailable={artifactAvailable}
              artifactButtonRef={artifactButtonRef}
              context={context}
              onOpenPalette={(opener) => {
                paletteOpenerRef.current = opener
              }}
            />
          </div>
        </header>
        <SurfaceHeaderTarget value={headerTarget}>{content}</SurfaceHeaderTarget>
      </main>
      {artifactOpen && !inspectorInSheet && (
        <aside className="product-inspector" aria-label="Inspector">
          <ArtifactInspector
            available={artifactAvailable}
            commandContext={context}
            digestInputRef={artifactDigestRef}
            onClose={closeArtifactInspector}
            state={artifactInspectorState}
            onStateChange={setArtifactInspectorState}
          />
        </aside>
      )}
      <OpenSessionDialog context={context} openerRef={paletteOpenerRef} fallbackRef={mainRef} />
      <CommandPalette
        context={context}
        openerRef={paletteOpenerRef}
        helpOpenerRef={helpOpenerRef}
        fallbackRef={mainRef}
      />
      <KeyboardHelp context={context} openerRef={helpOpenerRef} fallbackRef={mainRef} />
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
              const opener = navigationOpenerRef.current
              navigationOpenerRef.current = null
              if (opener?.isConnected && opener.getClientRects().length > 0) {
                event.preventDefault()
                opener.focus()
              }
            }}
          >
            <Dialog.Title className="sr-only">Product navigation</Dialog.Title>
            <Dialog.Description id="mobile-navigation-description" className="sr-only">
              Choose a page.
            </Dialog.Description>
            <Dialog.Close asChild>
              <button
                className="icon-button mobile-navigation-close"
                type="button"
                aria-label="Close navigation"
              >
                <X />
              </button>
            </Dialog.Close>
            <ProductNavigation
              active={surface}
              context={context}
              onActivate={() => dispatch(actions.overlaySet(null))}
            />
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
      <Dialog.Root
        open={artifactOpen && inspectorInSheet && app.overlay === null}
        onOpenChange={(open) => {
          if (!open && app.overlay === null) closeArtifactInspector()
        }}
      >
        <Dialog.Portal>
          <Dialog.Overlay className="dialog-overlay" />
          <Dialog.Content
            className="artifact-sheet"
            aria-describedby="artifact-sheet-description"
            onEscapeKeyDown={(event) => event.stopPropagation()}
            onOpenAutoFocus={(event) => {
              event.preventDefault()
              artifactDigestRef.current?.focus()
            }}
            onCloseAutoFocus={(event) => {
              event.preventDefault()
            }}
          >
            <Dialog.Title className="sr-only">Artifact inspector</Dialog.Title>
            <Dialog.Description id="artifact-sheet-description" className="sr-only">
              Look up a blob by digest.
            </Dialog.Description>
            <ArtifactInspector
              available={artifactAvailable}
              commandContext={context}
              digestInputRef={artifactDigestRef}
              onClose={closeArtifactInspector}
              state={artifactInspectorState}
              onStateChange={setArtifactInspectorState}
            />
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
    </div>
  )
}
