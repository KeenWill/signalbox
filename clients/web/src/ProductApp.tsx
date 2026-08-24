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
import { ArtifactInspector, useArtifactInspectorState } from './ArtifactInspector'
import { type CommandContext, globalHotkeyBindings, globalHotkeySequenceBindings } from './commands'
import { ArtifactRenderer } from './features/artifacts/ArtifactRenderer'
import type { ArtifactItem } from './features/artifacts/artifactTypes'
import { HttpImportApi } from './imports/api'
import { ImportsWorkspace } from './imports/ImportsWorkspace'
import {
  type ProductRouteId,
  productRoutes,
  productSurfaceStates,
  productTransport,
} from './product'
import {
  invokeProductCommand,
  type ProductCommandContext,
  productCommandRegistry,
  productHotkeySequenceBindings,
} from './productCommands'
import { SessionWorkspaceSurface } from './SessionWorkspaceSurface'
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

function ProductKeyboardHelp({ context }: { context: ProductCommandContext }) {
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
        <Dialog.Content className="dialog-content" aria-describedby="product-help-description">
          <div className="dialog-heading">
            <div>
              <Dialog.Title>Keyboard help</Dialog.Title>
              <Dialog.Description id="product-help-description">
                Product navigation and workstation shortcuts.
              </Dialog.Description>
            </div>
            <Dialog.Close asChild>
              <button className="icon-button" type="button" aria-label="Close Keyboard help">
                <X />
              </button>
            </Dialog.Close>
          </div>
          <dl className="shortcut-list">
            {productCommandRegistry
              .filter((command) => command.bindings.length > 0)
              .map((command) => (
                <div key={command.id}>
                  <dt>{command.title}</dt>
                  <dd>
                    {command.bindings.map((binding) => (
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

function ImportsCapabilityState({ state }: { state: 'pending' | 'failed' | 'unavailable' }) {
  const pending = state === 'pending'
  const failed = state === 'failed'
  return (
    <section className="surface-empty" aria-labelledby="imports-capability-heading">
      <AlertTriangle aria-hidden="true" />
      <div>
        <span className="availability-tag">
          {pending
            ? 'Checking capability'
            : failed
              ? 'Bootstrap unavailable'
              : 'Capability unavailable'}
        </span>
        <h2 id="imports-capability-heading">
          {pending
            ? 'Checking whether import discovery is available'
            : failed
              ? 'The daemon bootstrap contract could not be loaded'
              : 'Import discovery is not exposed by this daemon'}
        </h2>
        <p>
          {pending
            ? 'The imports workspace will open only after the bootstrap contract advertises it.'
            : failed
              ? 'Capability availability is unknown because the bootstrap request failed.'
              : 'The current bootstrap contract does not advertise imported-conversation discovery.'}
        </p>
      </div>
    </section>
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
}: {
  artifactAvailable: boolean
  artifactButtonRef: RefObject<HTMLButtonElement | null>
  context: ProductCommandContext
}) {
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
        onClick={() => invokeProductCommand('palette.open', context)}
      >
        <Command />
      </button>
    </div>
  )
}

const INSPECTOR_SHEET_MEDIA = '(max-width: 1080px)'

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

function SelectionInspector({ surface, title }: { surface: ProductRouteId; title: string }) {
  return (
    <>
      <span className="eyebrow">Inspector</span>
      <h2>Selection details</h2>
      <p>Select an available operational record to inspect its server-provided evidence.</p>
      <dl className="selection-inspector-details">
        <div>
          <dt>Surface</dt>
          <dd>{title}</dd>
        </div>
        <div>
          <dt>Authority</dt>
          <dd>{surface === 'settings' ? 'Browser' : 'Daemon'}</dd>
        </div>
        <div>
          <dt>Cache</dt>
          <dd>{surface === 'settings' ? 'Local settings' : 'Bounded query'}</dd>
        </div>
      </dl>
    </>
  )
}

export function ProductApp({ surface }: { surface: ProductRouteId }) {
  const dispatch = useAppDispatch()
  const app = useAppSelector(selectApp)
  const navigate = useNavigate()
  const primaryRef = useRef<HTMLElement>(null)
  const [firstTimelineWindow, setFirstTimelineWindow] = useState<(() => void) | null>(null)
  const [latestTimelineWindow, setLatestTimelineWindow] = useState<(() => void) | null>(null)
  const [timelineIds, setTimelineIds] = useState<readonly string[]>([])
  const [timelineSessionId, setTimelineSessionId] = useState<string | null>(null)
  const updateTimelineIds = useCallback((ids: readonly string[]) => setTimelineIds(ids), [])
  const artifactButtonRef = useRef<HTMLButtonElement>(null)
  const artifactDigestRef = useRef<HTMLInputElement>(null)
  const artifactSideWasOpen = useRef(false)
  const narrowInspector = useNarrowInspector()
  const artifactInspectorState = useArtifactInspectorState()
  const [importsCommandContext, setImportsCommandContext] = useState<CommandContext | null>(null)
  const updateImportsCommandContext = useCallback(
    (nextContext: CommandContext | null) => setImportsCommandContext(nextContext),
    [],
  )
  const updateFirstTimelineWindow = useCallback((action: (() => void) | null) => {
    setFirstTimelineWindow(() => action)
  }, [])
  const updateLatestTimelineWindow = useCallback((action: (() => void) | null) => {
    setLatestTimelineWindow(() => action)
  }, [])
  const bootstrap = useQuery({
    queryKey: ['production', 'bootstrap'],
    queryFn: ({ signal }) => productTransport.readBootstrap(signal),
    staleTime: Number.POSITIVE_INFINITY,
  })
  const artifactAvailable = bootstrap.data?.capabilities.immutable_blob_content === true
  const inspectorInSheet = app.layout === 'focus' || narrowInspector
  const context = useMemo<ProductCommandContext>(() => {
    const surfaceContext = surface === 'imports' ? importsCommandContext : null
    return {
      ...surfaceContext,
      dispatch,
      getState: store.getState,
      timelineIds: surfaceContext?.timelineIds ?? timelineIds,
      artifactPreviewIds: surfaceContext?.artifactPreviewIds ?? [],
      artifactOriginalIds: surfaceContext?.artifactOriginalIds ?? [],
      focusTimeline: surfaceContext?.focusTimeline ?? (() => primaryRef.current?.focus()),
      openFirstTimelineWindow: firstTimelineWindow ?? undefined,
      openLatestTimelineWindow: latestTimelineWindow ?? undefined,
      onTimelineSelected: (eventSequence) => {
        if (timelineSessionId !== null) {
          dispatch(
            actions.logicalPositionRecorded({
              sessionId: timelineSessionId,
              position: eventSequence,
            }),
          )
        }
      },
      navigate: (path) => {
        void navigate({ to: '/$surface', params: { surface: path.slice(1) } }).then(() => {
          primaryRef.current?.focus()
        })
      },
      openArtifactInspector: artifactAvailable
        ? () => dispatch(actions.overlaySet('artifact'))
        : undefined,
    }
  }, [
    artifactAvailable,
    dispatch,
    firstTimelineWindow,
    importsCommandContext,
    latestTimelineWindow,
    navigate,
    surface,
    timelineIds,
    timelineSessionId,
  ])
  useHotkeys(
    globalHotkeyBindings.map((binding) => ({
      hotkey: binding.hotkey,
      callback: () => {
        const overlay = store.getState().app.overlay
        if (overlay === null || binding.commandId === 'surface.escape') {
          invokeProductCommand(binding.commandId, context)
        }
      },
    })),
  )
  useHotkeySequences(
    [...globalHotkeySequenceBindings, ...productHotkeySequenceBindings].map((binding) => ({
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

  useEffect(() => {
    if (app.overlay === 'artifact' && !inspectorInSheet) {
      artifactSideWasOpen.current = true
      artifactDigestRef.current?.focus()
    } else if (artifactSideWasOpen.current && !inspectorInSheet) {
      artifactSideWasOpen.current = false
      artifactButtonRef.current?.focus()
    }
  }, [app.overlay, inspectorInSheet])

  const copy = surfaceCopy[surface]
  const importsAvailable = bootstrap.data?.capabilities.import_discovery === true
  const nonImportContent =
    surface === 'attention' ? (
      <AttentionSurface />
    ) : surface === 'sessions' ? (
      <SessionWorkspaceSurface
        onFirstWindowAction={updateFirstTimelineWindow}
        onLatestWindowAction={updateLatestTimelineWindow}
        onSessionId={setTimelineSessionId}
        onTimelineIds={updateTimelineIds}
      />
    ) : surface === 'settings' ? (
      <SettingsSurface />
    ) : surface === 'imports' ? (
      importsAvailable ? null : (
        <ImportsCapabilityState
          state={bootstrap.isPending ? 'pending' : bootstrap.isError ? 'failed' : 'unavailable'}
        />
      )
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
        <ProductNavigation active={surface} />
      </aside>
      <main className={`product-main product-main-${surface}`} tabIndex={-1} ref={primaryRef}>
        <header className="product-header">
          <div>
            <span className="eyebrow">{copy.eyebrow}</span>
            <h1>{copy.title}</h1>
          </div>
          <ProductToolbar
            artifactAvailable={artifactAvailable}
            artifactButtonRef={artifactButtonRef}
            context={context}
          />
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
        </div>
        {importsAvailable && (
          <div
            hidden={surface !== 'imports'}
            style={surface === 'imports' ? { display: 'contents' } : undefined}
          >
            <ImportsWorkspace
              api={productImportApi}
              scenario={false}
              active={surface === 'imports'}
              continuationAvailable={bootstrap.data?.capabilities.imported_continuations === true}
              presentation="product"
              onCommandContext={updateImportsCommandContext}
            />
          </div>
        )}
        {nonImportContent}
      </main>
      {app.layout === 'workbench' && (
        <aside className="product-inspector" aria-label="Inspector">
          {app.overlay === 'artifact' && !inspectorInSheet ? (
            <ArtifactInspector
              available={artifactAvailable}
              state={artifactInspectorState}
              digestInputRef={artifactDigestRef}
              onClose={() => dispatch(actions.overlaySet(null))}
            />
          ) : (
            <SelectionInspector surface={surface} title={copy.title} />
          )}
        </aside>
      )}
      <CommandPalette context={context} />
      <ProductKeyboardHelp context={context} />
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
      <Dialog.Root
        open={app.overlay === 'artifact' && inspectorInSheet}
        onOpenChange={(open) => {
          if (!open) dispatch(actions.overlaySet(null))
        }}
      >
        <Dialog.Portal>
          <Dialog.Overlay className="dialog-overlay" />
          <Dialog.Content
            className="artifact-sheet"
            aria-describedby="artifact-sheet-description"
            onOpenAutoFocus={(event) => {
              event.preventDefault()
              artifactDigestRef.current?.focus()
            }}
            onCloseAutoFocus={(event) => {
              event.preventDefault()
              artifactButtonRef.current?.focus()
            }}
          >
            <Dialog.Title className="sr-only">Artifact inspector</Dialog.Title>
            <Dialog.Description id="artifact-sheet-description" className="sr-only">
              Resolve and inspect an immutable Signalbox blob.
            </Dialog.Description>
            <ArtifactInspector
              available={artifactAvailable}
              state={artifactInspectorState}
              digestInputRef={artifactDigestRef}
              onClose={() => dispatch(actions.overlaySet(null))}
            />
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
    </div>
  )
}
