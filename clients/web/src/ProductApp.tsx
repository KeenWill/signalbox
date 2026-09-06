import * as Dialog from '@radix-ui/react-dialog'
import { useHotkeySequences, useHotkeys } from '@tanstack/react-hotkeys'
import { useQuery } from '@tanstack/react-query'
import { Link, useNavigate } from '@tanstack/react-router'
import {
  AlertTriangle,
  Command,
  FileSearch,
  Menu,
  Moon,
  PanelLeftClose,
  Rows3,
  Sun,
  X,
} from 'lucide-react'
import {
  type CSSProperties,
  type RefObject,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react'
import { ArtifactInspector, emptyArtifactInspectorState } from './ArtifactInspector'
import { AttentionSurface } from './AttentionSurface'
import type { CommandContext, CommandId } from './commands'
import { invokeCommand } from './commands'
import { ArtifactRenderer } from './features/artifacts/ArtifactRenderer'
import type { ArtifactItem } from './features/artifacts/artifactTypes'
import { HttpImportApi } from './imports/api'
import { ImportsWorkspace } from './imports/ImportsWorkspace'
import {
  ProductContractError,
  type ProductRouteId,
  type ProductRouteState,
  type ProductSearchState,
  type ProductSessionState,
  ProductTransportError,
  productRoutes,
  productSurfaceCacheLabel,
  productSurfaceStates,
  productTransport,
  readProductSessionState,
} from './product'
import {
  invokeProductCommand,
  type ProductCommandContext,
  productCommandAvailable,
  productCommandRegistry,
  productHotkeyBindings,
  productHotkeySequenceBindings,
} from './productCommands'
import { SearchSurface } from './SearchSurface'
import { SessionCatalogSurface } from './SessionCatalogSurface'
import { type SessionSelectionEvidence, SessionWorkspaceSurface } from './SessionWorkspaceSurface'
import { SettingsSurface } from './SettingsSurface'
import { hasValidSessionTimelineContract } from './session-timeline/model'
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

// A keystroke aimed at an editing control belongs to that control, never to a global binding.
const isEditableTarget = (target: EventTarget | null) => {
  if (!(target instanceof HTMLElement)) return false
  return (
    target.isContentEditable ||
    target instanceof HTMLInputElement ||
    target instanceof HTMLTextAreaElement ||
    target instanceof HTMLSelectElement
  )
}

const productNavigationCommandIds: Record<ProductRouteId, CommandId> = {
  attention: 'navigate.attention',
  sessions: 'navigate.sessions',
  search: 'navigate.search',
  runners: 'navigate.runners',
  reviews: 'navigate.reviews',
  imports: 'navigate.imports',
  usage: 'navigate.usage',
  settings: 'navigate.settings',
}

export function ProductNavigation({
  active,
  context,
  onActivate,
  disabled = false,
}: {
  active: ProductRouteId
  context: CommandContext
  onActivate?: () => void
  disabled?: boolean
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
              invokeCommand(productNavigationCommandIds[route.id], context)
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
          if (disabled) {
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
}: {
  context: ProductCommandContext
  openerRef: RefObject<HTMLElement | null>
}) {
  const open = useAppSelector((state) => state.app.overlay === 'palette')
  const focusTimelineAfterClose = useRef(false)
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
            if (focusTimelineAfterClose.current) {
              event.preventDefault()
              focusTimelineAfterClose.current = false
              context.focusTimeline()
              return
            }
            // Hand the palette's keystroke back to the control it was invoked from, unless another
            // overlay has already taken over the surface.
            const opener = openerRef.current
            if (context.getState().app.overlay !== null || !opener?.isConnected) return
            event.preventDefault()
            opener.focus()
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
                  command.id !== 'palette.open' &&
                  (!('available' in command) || command.available(context)),
              )
              .map((command) => (
                <button
                  key={command.id}
                  type="button"
                  onClick={() => {
                    focusTimelineAfterClose.current =
                      command.id.startsWith('selection.') &&
                      productCommandAvailable(command.id, context)
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

function DeferredSurface({ surface }: { surface: ProductRouteId }) {
  return (
    <div className="surface-body">
      <SurfaceUnavailable surface={surface} />
    </div>
  )
}

const reviewEvidenceUnavailable: ArtifactItem = {
  id: 'review-evidence-unavailable',
  displayName: 'Review evidence',
  kind: 'blocked',
  attemptedKind: 'review evidence artifact',
  reason: 'Review evidence is not exposed by the current daemon contract.',
}

function ReviewsArtifactSurface({ commandContext }: { commandContext: CommandContext }) {
  return (
    <div className="surface-body reviews-artifact-surface">
      <SurfaceUnavailable surface="reviews" />
      <section aria-labelledby="review-artifact-heading">
        <header>
          <span className="eyebrow">Typed artifact view</span>
          <h2 id="review-artifact-heading">Review evidence</h2>
          <p>
            Review facts and their artifact identities are not exposed by this daemon contract. The
            client preserves that missing typed boundary instead of fabricating a preview.
          </p>
        </header>
        <ArtifactRenderer artifact={reviewEvidenceUnavailable} commandContext={commandContext} />
      </section>
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

function useNarrowInspector(): boolean {
  const [narrow, setNarrow] = useState(() => window.matchMedia(INSPECTOR_SHEET_MEDIA).matches)
  useEffect(() => {
    const query = window.matchMedia(INSPECTOR_SHEET_MEDIA)
    const update = () => setNarrow(query.matches)
    query.addEventListener('change', update)
    return () => query.removeEventListener('change', update)
  }, [])
  return narrow
}

function SelectionInspector({
  cacheLabel,
  selectionEvidence,
  surface,
  title,
}: {
  cacheLabel: string | null
  selectionEvidence: SessionSelectionEvidence | null
  surface: ProductRouteId
  title: string
}) {
  return (
    <>
      <span className="eyebrow">Inspector</span>
      <h2>Selection details</h2>
      <p>
        {surface === 'settings'
          ? 'Presentation preferences are stored locally in this browser and do not represent server evidence.'
          : selectionEvidence === null
            ? 'Select an available operational record to inspect its server-provided evidence.'
            : 'Bounded server-provided timeline projection for the selected record.'}
      </p>
      <dl className="selection-inspector-details">
        <div>
          <dt>Surface</dt>
          <dd>{title}</dd>
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
    </>
  )
}

export function ProductApp({
  surface,
  search,
}: {
  surface: ProductRouteId
  search: ProductRouteState
}) {
  const dispatch = useAppDispatch()
  const app = useAppSelector(selectApp)
  const navigate = useNavigate()
  const mainRef = useRef<HTMLElement>(null)
  const timelineRef = useRef<HTMLDivElement>(null)
  const paletteOpenerRef = useRef<HTMLElement | null>(null)
  const navigationOpenerRef = useRef<HTMLElement | null>(null)
  const artifactButtonRef = useRef<HTMLButtonElement>(null)
  const artifactDigestRef = useRef<HTMLInputElement>(null)
  const bootstrapStatusRef = useRef<HTMLSpanElement>(null)
  const sessionState = useMemo(() => readProductSessionState({ ...search }), [search])
  const catalogSessionOpenedHere = useRef(false)
  const currentCatalogSession = useRef(sessionState.session)
  const artifactSideWasOpen = useRef(false)
  const inspectorWasInSheet = useRef(false)
  const surfaceEscapeRef = useRef<(() => boolean) | null>(null)
  const registerSurfaceEscape = useCallback((handler: (() => boolean) | null) => {
    surfaceEscapeRef.current = handler
  }, [])
  const [artifactOpen, setArtifactOpen] = useState(false)
  const [artifactInspectorState, setArtifactInspectorState] = useState(emptyArtifactInspectorState)
  const narrowInspector = useNarrowInspector()
  const [focusAfterBootstrapRecovery, setFocusAfterBootstrapRecovery] = useState(false)
  const [timelineIds, setTimelineIds] = useState<readonly string[]>([])
  const [timelineWindowAvailable, setTimelineWindowAvailable] = useState(false)
  const [selectionEvidence, setSelectionEvidence] = useState<SessionSelectionEvidence | null>(null)
  const [windowRequest, setWindowRequest] = useState<{
    anchor: 'first' | 'latest'
    attempt: number
  } | null>(null)
  const [importsCommandContext, setImportsCommandContext] = useState<CommandContext | null>(null)
  const [navigationDisabled, setNavigationDisabled] = useState(false)
  const updateImportsCommandContext = useCallback(
    (next: CommandContext | null) => setImportsCommandContext(next),
    [],
  )
  const updateTimelineIds = useCallback((ids: readonly string[]) => setTimelineIds(ids), [])
  const updateSelectionEvidence = useCallback(
    (evidence: SessionSelectionEvidence | null) => setSelectionEvidence(evidence),
    [],
  )
  const consumeWindowRequest = useCallback(() => setWindowRequest(null), [])
  const updateSessionSearch = useCallback(
    (next: ProductSessionState, mode: 'push' | 'close' = 'push') => {
      if (mode === 'close') {
        currentCatalogSession.current = next.session
        if (catalogSessionOpenedHere.current) {
          catalogSessionOpenedHere.current = false
          window.history.back()
          return
        }
        void navigate({ to: '/$surface', params: { surface }, search: next, replace: true })
        return
      }
      const previousSession = currentCatalogSession.current
      if (!previousSession && next.session) catalogSessionOpenedHere.current = true
      const switchesSelectedSession =
        previousSession !== undefined &&
        next.session !== undefined &&
        next.session !== previousSession
      currentCatalogSession.current = next.session
      void navigate({
        to: '/$surface',
        params: { surface },
        search: next,
        replace: switchesSelectedSession,
      })
    },
    [navigate, surface],
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
      ? 'Transport unavailable'
      : bootstrap.error instanceof ProductContractError
        ? 'Contract rejected'
        : 'Bootstrap unavailable'
    : null
  const inspectorInSheet = app.layout === 'focus' || narrowInspector
  // Imports reads and continuation mutations are admitted by the same bootstrap the shell validated.
  const productImportApi = useMemo(
    () =>
      bootstrap.data === undefined ? null : HttpImportApi.withAdmittedBootstrap(bootstrap.data),
    [bootstrap.data],
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
        if (surface === 'sessions' && sessionState.workspace) {
          updateSessionSearch({ ...sessionState, workspace: undefined }, 'close')
          return true
        }
        if (surface === 'sessions' && sessionState.session) {
          updateSessionSearch({ ...sessionState, session: undefined }, 'close')
          return true
        }
        return surfaceEscapeRef.current?.() ?? false
      },
      openArtifactInspector: artifactAvailable ? () => setArtifactOpen(true) : undefined,
      loadTimelineWindow: (anchor) =>
        setWindowRequest((current) => ({ anchor, attempt: (current?.attempt ?? 0) + 1 })),
      navigate: (path) => {
        // A retained exact continuation command owns the surface until it is retried or abandoned.
        if (navigationDisabled) return
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
    artifactAvailable,
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
  const artifactSheetOwnsFocus = artifactOpen && inspectorInSheet
  useHotkeys(
    productHotkeyBindings.map((binding) => ({
      hotkey: binding.hotkey,
      // Product surfaces own text fields, so the palette binding must never steal a keystroke the
      // field is editing.
      options: binding.commandId === 'palette.open' ? { ignoreInputs: true } : undefined,
      callback: (event) => {
        if (artifactSheetOwnsFocus) return
        if (
          (binding.commandId === 'palette.open' || binding.commandId === 'surface.escape') &&
          isEditableTarget(event.target)
        ) {
          return
        }
        if (store.getState().app.overlay === null || binding.commandId === 'surface.escape') {
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
    productHotkeySequenceBindings.map((binding) => ({
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
    if (!focusAfterBootstrapRecovery || !bootstrap.isSuccess) return
    setFocusAfterBootstrapRecovery(false)
    const frame = requestAnimationFrame(() => mainRef.current?.focus())
    return () => cancelAnimationFrame(frame)
  }, [bootstrap.isSuccess, focusAfterBootstrapRecovery])

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

  const copy = surfaceCopy[surface]
  const cacheLabel = productSurfaceCacheLabel(surface)
  const timelineCapability = bootstrap.isPending
    ? 'checking'
    : bootstrap.isSuccess && hasValidSessionTimelineContract(bootstrap.data)
      ? 'available'
      : 'unavailable'

  useEffect(() => {
    const previousTitle = document.title
    document.title = `${copy.title} · Signalbox`
    return () => {
      document.title = previousTitle
    }
  }, [copy.title])

  const updateSearch = (next: ProductSearchState) =>
    void navigate({ to: '/$surface', params: { surface }, search: next })

  const content =
    surface === 'attention' && bootstrap.isSuccess ? (
      <AttentionSurface registerEscapeHandler={registerSurfaceEscape} />
    ) : surface === 'attention' ? (
      <div className="surface-body">
        <section className="surface-empty" role={bootstrap.isError ? 'alert' : 'status'}>
          <div>
            <h2>
              {bootstrap.isError ? 'Attention contract unavailable' : 'Checking Attention contract'}
            </h2>
            <p>
              {bootstrap.isError
                ? 'Attention reads remain disabled until the generated bootstrap contract validates.'
                : 'Attention reads will begin after the generated bootstrap contract validates.'}
            </p>
            {bootstrap.isError && (
              <button
                type="button"
                className="bootstrap-retry"
                onClick={() => {
                  setFocusAfterBootstrapRecovery(true)
                  void bootstrap.refetch()
                }}
              >
                Retry contract check
              </button>
            )}
          </div>
        </section>
      </div>
    ) : surface === 'sessions' && bootstrap.isSuccess && sessionState.workspace ? (
      <SessionWorkspaceSurface
        initialSessionId={sessionState.session}
        onSelectionEvidence={updateSelectionEvidence}
        onTimelineIds={updateTimelineIds}
        onTimelineWindowAvailable={setTimelineWindowAvailable}
        onWindowRequestConsumed={consumeWindowRequest}
        timelineCapability={timelineCapability}
        timelineRef={timelineRef}
        windowRequest={windowRequest}
      />
    ) : surface === 'sessions' && bootstrap.isSuccess ? (
      <SessionCatalogSurface state={sessionState} onStateChange={updateSessionSearch} />
    ) : surface === 'sessions' ? (
      <div className="catalog-notice">
        <p>Sessions are unavailable until the browser contract handshake succeeds.</p>
        {bootstrap.isError && (
          <button
            type="button"
            onClick={() => {
              setFocusAfterBootstrapRecovery(true)
              void bootstrap.refetch().then((result) => {
                if (result.isSuccess) requestAnimationFrame(() => mainRef.current?.focus())
              })
            }}
          >
            Retry contract handshake
          </button>
        )}
      </div>
    ) : surface === 'search' && bootstrap.isError ? (
      <div className="surface-body">
        <section className="surface-empty" role="alert">
          <AlertTriangle aria-hidden="true" />
          <div>
            <h2>Search availability could not be checked</h2>
            <p>
              {bootstrap.error instanceof ProductContractError
                ? 'The daemon response is incompatible with the generated web contract.'
                : 'Signalbox could not be reached. Retry the contract check when transport is available.'}
            </p>
          </div>
        </section>
      </div>
    ) : surface === 'search' && bootstrap.data === undefined ? (
      <div className="surface-body">
        <p className="search-notice">Checking whether bounded search is available…</p>
      </div>
    ) : surface === 'search' &&
      (bootstrap.data?.capabilities.bounded_json === false ||
        bootstrap.data?.capabilities.bounded_lexical_search === false) ? (
      <div className="surface-body">
        <section className="surface-empty" aria-labelledby="search-unavailable-heading">
          <AlertTriangle aria-hidden="true" />
          <div>
            <span className="availability-tag">Committed · unavailable</span>
            <h2 id="search-unavailable-heading">
              This daemon contract does not advertise bounded lexical search
            </h2>
            <p>
              The search surface is ready, but the connected daemon does not expose the bounded
              search capability. Signalbox will not infer or fabricate results.
            </p>
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
            <span className="availability-tag">Contract required</span>
            <h2 id="imports-unavailable-heading">
              Imports are unavailable until bootstrap admission succeeds
            </h2>
            <p>
              Signalbox will not issue import reads or enable continuation mutations without an
              admitted daemon contract.
            </p>
          </div>
        </section>
      </div>
    ) : surface === 'reviews' ? (
      <ReviewsArtifactSurface commandContext={context} />
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
        <ProductNavigation active={surface} context={context} disabled={navigationDisabled} />
      </aside>
      <main className={`product-main product-main-${surface}`} ref={mainRef} tabIndex={-1}>
        <header className="product-header">
          <div>
            <span className="eyebrow">{copy.eyebrow}</span>
            <h1>{copy.title}</h1>
          </div>
          <ProductToolbar
            artifactAvailable={artifactAvailable}
            artifactButtonRef={artifactButtonRef}
            context={context}
            onOpenPalette={(opener) => {
              paletteOpenerRef.current = opener
            }}
          />
        </header>
        <div className="surface-question">
          <p>{copy.question}</p>
          {surface === 'settings' ? (
            <span className="contract-state ready" role="status">
              Browser-local preferences
            </span>
          ) : (
            <span
              ref={bootstrapStatusRef}
              className={`contract-state ${bootstrap.isSuccess ? 'ready' : bootstrap.isError ? 'failed' : ''}`}
              role="status"
              aria-live="polite"
              aria-atomic="true"
              tabIndex={-1}
            >
              {bootstrap.isSuccess
                ? `${bootstrap.data.contract.name} · ${bootstrap.data.contract.version}`
                : bootstrap.isError
                  ? bootstrapFailure
                  : 'Checking contract…'}
            </span>
          )}
          {surface !== 'settings' && bootstrap.isError && (
            <button
              type="button"
              className="bootstrap-retry"
              onClick={(event) => {
                const restoreFocus = document.activeElement === event.currentTarget
                void bootstrap.refetch().then((result) => {
                  if (result.isSuccess && restoreFocus) {
                    requestAnimationFrame(() => bootstrapStatusRef.current?.focus())
                  }
                })
              }}
            >
              Retry bootstrap
            </button>
          )}
        </div>
        {content}
      </main>
      {app.layout === 'workbench' && (
        <aside className="product-inspector" aria-label="Inspector">
          {artifactOpen && !inspectorInSheet ? (
            <ArtifactInspector
              available={artifactAvailable}
              commandContext={context}
              digestInputRef={artifactDigestRef}
              onClose={closeArtifactInspector}
              state={artifactInspectorState}
              onStateChange={setArtifactInspectorState}
            />
          ) : (
            <SelectionInspector
              cacheLabel={cacheLabel}
              selectionEvidence={selectionEvidence}
              surface={surface}
              title={copy.title}
            />
          )}
        </aside>
      )}
      <CommandPalette context={context} openerRef={paletteOpenerRef} />
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
              const opener = navigationOpenerRef.current
              navigationOpenerRef.current = null
              if (opener?.isConnected) {
                event.preventDefault()
                opener.focus()
              }
            }}
          >
            <Dialog.Title className="sr-only">Product navigation</Dialog.Title>
            <Dialog.Description id="mobile-navigation-description" className="sr-only">
              Choose a Signalbox surface.
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
              disabled={navigationDisabled}
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
              Resolve and inspect an immutable Signalbox blob.
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
