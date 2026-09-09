import { useHotkeySequences, useHotkeys } from '@tanstack/react-hotkeys'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useNavigate } from '@tanstack/react-router'
import { Menu } from 'lucide-react'
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  type CommandContext,
  importHotkeyBindings,
  importHotkeySequenceBindings,
  invokeCommand,
  surfaceHotkeyBindings,
  surfaceHotkeySequenceBindings,
} from '../commands'
import type {
  WebImportContinuationReference,
  WebImportContinuationRequest,
  WebImportEntryWindowRequest,
  WebImportedSessionRelationship,
  WebImportFormat,
} from '../generated/web-contract.mjs'
import { ScenarioNavigation } from '../ScenarioNavigation'
import { type DiagnosticSnapshot, IconCommand, OverlaySurfaces } from '../Surfaces'
import { selectApp, store, useAppSelector } from '../state'
import {
  correlateImportDescriptor,
  correlateImportFrontiers,
  type ImportApi,
  ImportApiError,
  ImportReceiptCorrelationError,
} from './api'
import { ImportedArtifactView } from './ImportedArtifactView'
import { ImportedEntries } from './ImportedEntries'
import { ImportsTable } from './ImportsTable'
import { loadRetainedCommand, storeRetainedCommand } from './retainedCommand'
import { SCENARIO_IMPORT_TOTAL } from './scenario'

const IMPORT_PAGE_ITEMS = 100
const IMPORT_WINDOW_RADIUS = 50
const EMPTY_FILTER = ''
const SCENARIO_MODEL_SELECTION = '00000000-0000-7000-8000-000000000777'
const CANONICAL_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/
const NIL_UUID = '00000000-0000-0000-0000-000000000000'

type FormatFilter = WebImportFormat | typeof EMPTY_FILTER
type ModelKind = 'direct' | 'alias'

const formatOptions: ReadonlyArray<{ value: FormatFilter; label: string }> = [
  { value: EMPTY_FILTER, label: 'All source formats' },
  { value: 'claude_code_session_jsonl_v2', label: 'Claude Code · converter 2' },
  { value: 'claude_code_session_jsonl_v1', label: 'Claude Code · converter 1' },
  { value: 'codex_rollout_jsonl_v1', label: 'Codex rollout · converter 1' },
]

const DEFINITIVE_CONTINUATION_ERRORS = new Set([
  'conflicting_command_reuse',
  'import_frontier_not_found',
  'import_not_found',
  'invalid_import_request',
  'model_not_configured',
])

const isRetryableContinuationError = (error: unknown): boolean =>
  error instanceof ImportReceiptCorrelationError ||
  !(error instanceof ImportApiError) ||
  !DEFINITIVE_CONTINUATION_ERRORS.has(error.detail.error.code)

const byteLabel = (bytes: number): string => {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`
}

export function ImportsWorkspace({
  api,
  scenario,
  presentation = 'standalone',
  onCommandContext,
  onNavigationDisabledChange,
}: {
  api: ImportApi
  scenario: boolean
  // `standalone` renders this surface's own navigation, header, and overlays. `product` mounts the
  // same surface inside the product shell, which already owns that chrome and the command registry.
  presentation?: 'standalone' | 'product'
  onCommandContext?: (context: CommandContext | null) => void
  onNavigationDisabledChange?: (disabled: boolean) => void
}) {
  const queryClient = useQueryClient()
  const catalogRef = useRef<HTMLElement>(null)
  const navigate = useNavigate()
  const app = useAppSelector(selectApp)
  const queryScope = scenario ? 'scenario' : 'production'
  useEffect(() => {
    if (!scenario) return
    const previousTitle = document.title
    document.title = 'Signalbox Scenario Studio — Imports'
    return () => {
      document.title = previousTitle
    }
  }, [scenario])
  useEffect(() => {
    document.documentElement.dataset.theme = app.theme
    document.documentElement.dataset.density = app.density
  }, [app.density, app.theme])
  const [format, setFormat] = useState<FormatFilter>(EMPTY_FILTER)
  const [sourceSession, setSourceSession] = useState('')
  const [settledSourceSession, setSettledSourceSession] = useState(sourceSession)
  useEffect(() => {
    const timer = window.setTimeout(() => {
      setAfter(undefined)
      setSettledSourceSession(sourceSession)
    }, 200)
    return () => window.clearTimeout(timer)
  }, [sourceSession])
  const [sourceSessionFilterEnabled, setSourceSessionFilterEnabled] = useState(false)
  const [after, setAfter] = useState<string | undefined>()
  const [selectedImport, setSelectedImport] = useState<string | null>(null)
  const [windowRequest, setWindowRequest] = useState<WebImportEntryWindowRequest>({
    anchor: 'first',
    before: 0,
    after: IMPORT_WINDOW_RADIUS,
  })
  const [positionInput, setPositionInput] = useState('')
  const [selectedFrontier, setSelectedFrontier] = useState<WebImportContinuationReference | null>(
    null,
  )
  const [pendingCommand, setPendingCommand] = useState<WebImportContinuationRequest | null>(() =>
    loadRetainedCommand(queryScope),
  )
  const [modelKind, setModelKind] = useState<ModelKind>(
    () => pendingCommand?.initial_model_selection.kind ?? 'direct',
  )
  const [modelSelectionId, setModelSelectionId] = useState(() => {
    const retainedModel = pendingCommand?.initial_model_selection
    if (retainedModel?.kind === 'direct') return retainedModel.selection_id
    if (retainedModel?.kind === 'alias') return retainedModel.alias_id
    return scenario ? SCENARIO_MODEL_SELECTION : ''
  })
  const [retainedStorageFailed, setRetainedStorageFailed] = useState(false)
  const hasRetainedCommand = pendingCommand !== null

  useEffect(() => {
    onNavigationDisabledChange?.(hasRetainedCommand)
  }, [hasRetainedCommand, onNavigationDisabledChange])

  const listRequest = useMemo(
    () => ({
      after,
      limit: IMPORT_PAGE_ITEMS,
      format: format || undefined,
      source_session_id: sourceSessionFilterEnabled ? settledSourceSession : undefined,
    }),
    [after, format, settledSourceSession, sourceSessionFilterEnabled],
  )
  const importsQuery = useQuery({
    queryKey: ['imports', queryScope, 'catalog', listRequest],
    queryFn: ({ signal }) => api.list(listRequest, signal),
    gcTime: 0,
  })
  const imports = importsQuery.isError ? undefined : importsQuery.data
  const firstImport = imports?.items[0]?.imported_conversation_id ?? null

  useEffect(() => {
    if (
      imports !== undefined &&
      !hasRetainedCommand &&
      (!selectedImport ||
        !imports?.items.some((item) => item.imported_conversation_id === selectedImport))
    ) {
      setSelectedImport(firstImport)
      setWindowRequest({ anchor: 'first', before: 0, after: IMPORT_WINDOW_RADIUS })
      setSelectedFrontier(null)
      setPositionInput('')
    }
  }, [firstImport, hasRetainedCommand, imports, selectedImport])

  const selectedSummary = imports?.items.find(
    (item) => item.imported_conversation_id === selectedImport,
  )
  const descriptorQuery = useQuery({
    queryKey: ['imports', queryScope, selectedImport, 'descriptor', selectedSummary],
    queryFn: async ({ signal }) => {
      if (!selectedSummary) throw new Error('Selected import is absent from the catalog')
      return correlateImportDescriptor(
        await api.descriptor(selectedSummary.imported_conversation_id, signal),
        selectedSummary,
      )
    },
    enabled: selectedSummary !== undefined,
    gcTime: 0,
  })
  const descriptor =
    importsQuery.isError || descriptorQuery.isError ? undefined : descriptorQuery.data
  const windowQuery = useQuery({
    queryKey: [
      'imports',
      queryScope,
      selectedImport,
      'entries',
      windowRequest,
      descriptor?.timeline,
    ],
    queryFn: async ({ signal }) => {
      if (!descriptor) throw new Error('Import descriptor is unavailable')
      return correlateImportFrontiers(
        await api.entries(
          selectedImport ?? '',
          windowRequest,
          signal,
          descriptor.timeline.latest.position,
        ),
        descriptor.timeline,
      )
    },
    gcTime: 0,
    enabled: selectedImport !== null && descriptor !== undefined,
  })
  const entryWindow =
    importsQuery.isError || descriptorQuery.isError || windowQuery.isError
      ? undefined
      : windowQuery.data
  const selectedEntry =
    entryWindow?.items.find(
      (entry) => entry.frontier.imported_entry_id === selectedFrontier?.imported_entry_id,
    ) ?? null
  const anchorFrontier =
    entryWindow?.items.find((entry) => entry.frontier.position === entryWindow.anchor_position)
      ?.frontier ?? null
  const importEntryIds = useMemo(
    () => entryWindow?.items.map((entry) => entry.frontier.imported_entry_id) ?? [],
    [entryWindow?.items],
  )

  // A refetch rebuilds `anchorFrontier` identity without changing the window, so only fall back to
  // the anchor when the operator's selection is absent from the returned window.
  useEffect(() => {
    if (hasRetainedCommand) return
    setSelectedFrontier((current) =>
      current !== null &&
      entryWindow?.items.some(
        (entry) => entry.frontier.imported_entry_id === current.imported_entry_id,
      )
        ? current
        : anchorFrontier,
    )
  }, [anchorFrontier, entryWindow?.items, hasRetainedCommand])

  const continuation = useMutation({
    mutationFn: (request: WebImportContinuationRequest) =>
      api.continueImport(request.frontier.imported_conversation_id, request),
    onSuccess: () => {
      storeRetainedCommand(queryScope, null)
      setPendingCommand(null)
    },
    onError: (error) => {
      if (!isRetryableContinuationError(error)) {
        storeRetainedCommand(queryScope, null)
        setPendingCommand(null)
      }
    },
  })
  const resetContinuation = continuation.reset
  const selectImportEntry = useCallback(
    (id: string) => {
      if (hasRetainedCommand || store.getState().app.overlay !== null) return
      resetContinuation()
      setSelectedFrontier(
        entryWindow?.items.find((entry) => entry.frontier.imported_entry_id === id)?.frontier ??
          null,
      )
    },
    [entryWindow?.items, hasRetainedCommand, resetContinuation],
  )
  const normalizedModelSelectionId = modelSelectionId.trim()
  const modelSelectionInvalid =
    !CANONICAL_UUID.test(normalizedModelSelectionId) || normalizedModelSelectionId === NIL_UUID
  const canContinueImport =
    selectedFrontier !== null &&
    !modelSelectionInvalid &&
    !importsQuery.isError &&
    !descriptorQuery.isError &&
    !windowQuery.isError &&
    !continuation.isPending &&
    !hasRetainedCommand
  const continueAt = useCallback(
    (relationship: WebImportedSessionRelationship) => {
      if (!canContinueImport || !selectedFrontier || store.getState().app.overlay !== null) return
      const request: WebImportContinuationRequest = {
        command_id: crypto.randomUUID(),
        frontier: selectedFrontier,
        relationship,
        initial_model_selection:
          modelKind === 'direct'
            ? { kind: 'direct', selection_id: normalizedModelSelectionId }
            : { kind: 'alias', alias_id: normalizedModelSelectionId },
      }
      if (!storeRetainedCommand(queryScope, request)) {
        setRetainedStorageFailed(true)
        return
      }
      setRetainedStorageFailed(false)
      setPendingCommand(request)
      continuation.mutate(request)
    },
    [
      canContinueImport,
      continuation,
      modelKind,
      normalizedModelSelectionId,
      queryScope,
      selectedFrontier,
    ],
  )
  const retryExactCommand = useCallback(() => {
    if (!pendingCommand || continuation.isPending) return
    continuation.mutate(pendingCommand)
  }, [continuation, pendingCommand])
  const abandonExactCommand = useCallback(() => {
    if (!pendingCommand || continuation.isPending) return
    storeRetainedCommand(queryScope, null)
    setPendingCommand(null)
    continuation.reset()
  }, [continuation, pendingCommand, queryScope])
  const canRecoverRetainedCommand = pendingCommand !== null && !continuation.isPending
  const refetchImports = importsQuery.refetch
  const retryImportDiscovery = useCallback(() => {
    catalogRef.current?.focus()
    void refetchImports()
  }, [refetchImports])
  const commandContext = useMemo<CommandContext>(
    () => ({
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () =>
        document.querySelector<HTMLElement>('[aria-label="Imported source entries"]')?.focus(),
      importEntryIds,
      selectedImportEntry: selectedFrontier?.imported_entry_id ?? null,
      // Keep navigation commands discoverable while the command palette is open.
      // Execution still checks the live overlay state in selectImportEntry.
      canSelectImportEntry: !hasRetainedCommand,
      selectImportEntry,
      canContinueImport,
      continueImport: continueAt,
      canRetryImport: canRecoverRetainedCommand,
      retryImport: retryExactCommand,
      retryImportDiscovery: importsQuery.isError ? retryImportDiscovery : undefined,
      canAbandonImport: canRecoverRetainedCommand,
      abandonImport: abandonExactCommand,
      navigate: (path) => void navigate({ to: '/$surface', params: { surface: path.slice(1) } }),
    }),
    [
      abandonExactCommand,
      canContinueImport,
      canRecoverRetainedCommand,
      continueAt,
      hasRetainedCommand,
      importEntryIds,
      importsQuery.isError,
      navigate,
      retryExactCommand,
      retryImportDiscovery,
      selectImportEntry,
      selectedFrontier?.imported_entry_id,
    ],
  )
  useEffect(() => {
    onCommandContext?.(commandContext)
    return () => onCommandContext?.(null)
  }, [commandContext, onCommandContext])
  // The product shell registers every product command itself, so publishing the surface context is
  // the whole seam there. Registering these bindings again would advance the selection twice.
  useHotkeys(
    (presentation === 'standalone' && app.overlay === null
      ? [...surfaceHotkeyBindings, ...(hasRetainedCommand ? [] : importHotkeyBindings)]
      : []
    ).map((binding) => ({
      hotkey: binding.hotkey,
      callback: () => invokeCommand(binding.commandId, commandContext),
    })),
  )
  useHotkeySequences(
    (presentation === 'standalone' && app.overlay === null
      ? [
          ...(hasRetainedCommand ? [] : surfaceHotkeySequenceBindings),
          ...(hasRetainedCommand ? [] : importHotkeySequenceBindings),
        ]
      : []
    ).map((binding) => ({
      sequence: binding.sequence,
      callback: () => invokeCommand(binding.commandId, commandContext),
    })),
  )

  useEffect(() => {
    if (!scenario) return
    const snapshot: DiagnosticSnapshot = {
      scenario: 'imports',
      connection:
        importsQuery.isError || descriptorQuery.isError || windowQuery.isError ? 'failed' : 'ready',
      loadedTimeline: 0,
      logicalTimeline: 0,
      loadedFleet: 0,
      logicalFleet: 0,
      transcriptRange: { start: 0, end: 0 },
      tableRange: { start: 0, end: 0 },
      queryStates: [
        `imports: ${importsQuery.status}/${importsQuery.fetchStatus}`,
        `descriptor: ${descriptorQuery.status}/${descriptorQuery.fetchStatus}`,
        `entries: ${windowQuery.status}/${windowQuery.fetchStatus}`,
      ],
      queryCacheSize: queryClient.getQueryCache().getAll().length,
      recentActions: [],
      loadedImports: imports?.items.length ?? 0,
      logicalImports: scenario ? SCENARIO_IMPORT_TOTAL : undefined,
      loadedImportEntries: entryWindow?.items.length ?? 0,
      selectedImport,
      selectedImportPosition: selectedFrontier?.position ?? null,
    }
    window.__SIGNALBOX_DIAGNOSTICS__ = () => snapshot
    return () => {
      delete window.__SIGNALBOX_DIAGNOSTICS__
    }
  }, [
    descriptorQuery.fetchStatus,
    descriptorQuery.isError,
    descriptorQuery.status,
    imports?.items.length,
    importsQuery.fetchStatus,
    importsQuery.isError,
    importsQuery.status,
    queryClient,
    scenario,
    selectedFrontier?.position,
    selectedImport,
    entryWindow?.items.length,
    windowQuery.fetchStatus,
    windowQuery.isError,
    windowQuery.status,
  ])

  const resetCatalog = () => {
    if (hasRetainedCommand) return
    resetContinuation()
    setAfter(undefined)
    setSelectedImport(null)
  }

  const showCatalogPage = (cursor: string | undefined) => {
    if (hasRetainedCommand) return
    resetContinuation()
    setAfter(cursor)
  }

  const showWindow = (request: WebImportEntryWindowRequest) => {
    if (hasRetainedCommand) return
    if (
      request.anchor === windowRequest.anchor &&
      request.position === windowRequest.position &&
      request.before === windowRequest.before &&
      request.after === windowRequest.after
    )
      return
    resetContinuation()
    setWindowRequest(request)
    setSelectedFrontier(null)
  }

  const requestedPosition = Number(positionInput)
  const requestedPositionInRange =
    positionInput.trim() !== '' &&
    Number.isSafeInteger(requestedPosition) &&
    requestedPosition > 0 &&
    requestedPosition <= (descriptor?.entry_count ?? 0)

  const showPosition = () => {
    const position = Number(positionInput)
    if (
      !Number.isSafeInteger(position) ||
      position <= 0 ||
      position > (descriptor?.entry_count ?? 0)
    )
      return
    showWindow({
      anchor: 'position',
      position,
      before: Math.floor(IMPORT_WINDOW_RADIUS / 2),
      after: Math.floor(IMPORT_WINDOW_RADIUS / 2),
    })
  }

  const selectImport = (importedConversationId: string) => {
    if (hasRetainedCommand) return
    resetContinuation()
    setSelectedImport(importedConversationId)
    setWindowRequest({ anchor: 'first', before: 0, after: IMPORT_WINDOW_RADIUS })
    setSelectedFrontier(null)
    setPositionInput('')
  }

  const retryableContinuationFailure =
    continuation.isError && isRetryableContinuationError(continuation.error)
  const retainedCommandNeedsAction = pendingCommand !== null && !continuation.isPending
  const inspectorVisible =
    selectedImport !== null || hasRetainedCommand || continuation.isSuccess || continuation.isError

  return (
    <>
      <div
        className={`imports-shell imports-shell-${presentation} ${inspectorVisible ? 'inspector-open' : ''}`}
      >
        {presentation === 'standalone' && (
          <aside className="navigation-pane imports-navigation">
            <ScenarioNavigation
              activeId={scenario ? 'imports' : 'production-imports'}
              disabled={hasRetainedCommand}
            />
          </aside>
        )}
        <div
          className={`imports-workspace imports-workspace-${presentation} ${inspectorVisible ? 'inspector-open' : ''}`}
          role={presentation === 'standalone' ? 'main' : undefined}
        >
          {presentation === 'standalone' && (
            <header className="imports-header">
              <IconCommand
                id="navigation.open"
                context={commandContext}
                label="Open scenarios"
                className="icon-button imports-mobile-navigation"
              >
                <Menu />
              </IconCommand>
              <div>
                <h1>Imports</h1>
              </div>
            </header>
          )}
          <section
            ref={catalogRef}
            tabIndex={-1}
            className="imports-catalog"
            aria-labelledby="imports-catalog-heading"
          >
            <header className="section-header imports-catalog-header">
              <h2 id="imports-catalog-heading" className="sr-only">
                Imports
              </h2>
              <div className="imports-filters">
                <label>
                  <span>Format</span>
                  <select
                    aria-label="Filter imports by format"
                    value={format}
                    disabled={hasRetainedCommand}
                    onChange={(event) => {
                      if (hasRetainedCommand) return
                      setFormat(event.target.value as FormatFilter)
                      resetCatalog()
                    }}
                  >
                    {formatOptions.map((option) => (
                      <option value={option.value} key={option.value || 'all'}>
                        {option.label}
                      </option>
                    ))}
                  </select>
                </label>
                <label>
                  <span>Source session</span>
                  <input
                    aria-label="Source session"
                    value={sourceSession}
                    placeholder="Source session ID"
                    disabled={hasRetainedCommand}
                    onChange={(event) => {
                      if (hasRetainedCommand) return
                      setSourceSession(event.target.value)
                      resetCatalog()
                    }}
                  />
                </label>
                <label className="source-session-filter-toggle">
                  <span>Filter by source</span>
                  <input
                    aria-label="Filter by source"
                    type="checkbox"
                    checked={sourceSessionFilterEnabled}
                    disabled={hasRetainedCommand}
                    onChange={(event) => {
                      if (hasRetainedCommand) return
                      setSourceSessionFilterEnabled(event.target.checked)
                      resetCatalog()
                    }}
                  />
                </label>
                <button
                  type="button"
                  disabled={!after || hasRetainedCommand}
                  onClick={() => showCatalogPage(undefined)}
                >
                  First
                </button>
                <button
                  type="button"
                  disabled={!imports?.next_cursor || hasRetainedCommand}
                  onClick={() => showCatalogPage(imports?.next_cursor ?? undefined)}
                >
                  Next
                </button>
              </div>
            </header>
            {importsQuery.isPending && <p className="imports-state">Loading imports…</p>}
            {importsQuery.isError && (
              <div className="imports-state imports-error" role="alert">
                <p>Imports unavailable</p>
                <button
                  type="button"
                  onClick={() => invokeCommand('imports.discovery.retry', commandContext)}
                >
                  Retry imports
                </button>
              </div>
            )}
            {imports && (
              <ImportsTable
                rows={imports.items}
                selectedId={selectedImport}
                selectionDisabled={hasRetainedCommand}
                onSelect={selectImport}
              />
            )}
          </section>
          {inspectorVisible && (
            <section className="import-inspector" aria-labelledby="import-inspector-heading">
              <header className="section-header import-inspector-header">
                <div>
                  <h2 id="import-inspector-heading">
                    {descriptor?.display_title ?? 'Import inspector'}
                  </h2>
                </div>
                <div className="window-controls">
                  <button
                    type="button"
                    disabled={hasRetainedCommand}
                    onClick={() =>
                      showWindow({ anchor: 'first', before: 0, after: IMPORT_WINDOW_RADIUS })
                    }
                  >
                    First
                  </button>
                  <button
                    type="button"
                    disabled={hasRetainedCommand}
                    onClick={() =>
                      showWindow({ anchor: 'latest', before: IMPORT_WINDOW_RADIUS, after: 0 })
                    }
                  >
                    Latest
                  </button>
                  <label>
                    <span>Position</span>
                    <input
                      aria-label="Imported entry position"
                      inputMode="numeric"
                      value={positionInput}
                      disabled={hasRetainedCommand}
                      onChange={(event) => {
                        if (hasRetainedCommand) return
                        setPositionInput(event.target.value)
                      }}
                    />
                  </label>
                  <button
                    type="button"
                    disabled={hasRetainedCommand || !requestedPositionInRange}
                    onClick={showPosition}
                  >
                    Go
                  </button>
                </div>
              </header>
              <div className="import-inspector-body">
                <div className="import-evidence">
                  {descriptor && (
                    <dl>
                      <div>
                        <dt>Import ID</dt>
                        <dd>{descriptor.imported_conversation_id}</dd>
                      </div>
                      <div>
                        <dt>Format</dt>
                        <dd>{descriptor.source.format}</dd>
                      </div>
                      <div>
                        <dt>Source session</dt>
                        <dd>
                          {descriptor.source.source_session_id
                            ? `${descriptor.source.source_session_id.leading_text}${
                                descriptor.source.source_session_id.completeness === 'truncated'
                                  ? '…'
                                  : ''
                              }`
                            : 'Not attested'}
                        </dd>
                      </div>
                      <div>
                        <dt>Source digest</dt>
                        <dd>{descriptor.source.source_digest_sha256}</dd>
                      </div>
                      <div>
                        <dt>Raw records</dt>
                        <dd>{descriptor.raw_record_count.toLocaleString()}</dd>
                      </div>
                      <div>
                        <dt>Entries</dt>
                        <dd>{descriptor.entry_count.toLocaleString()}</dd>
                      </div>
                      <div>
                        <dt>Raw source size</dt>
                        <dd>{byteLabel(descriptor.sizes.raw_source_bytes)}</dd>
                      </div>
                      <div>
                        <dt>Normalized records</dt>
                        <dd>{byteLabel(descriptor.sizes.normalized_source_record_bytes)}</dd>
                      </div>
                      <div>
                        <dt>Normalized entries</dt>
                        <dd>{byteLabel(descriptor.sizes.normalized_entry_bytes)}</dd>
                      </div>
                      <div>
                        <dt>Timeline</dt>
                        <dd>1–{descriptor.timeline.latest.position.toLocaleString()}</dd>
                      </div>
                    </dl>
                  )}
                  {descriptorQuery.isError && (
                    <p className="imports-state" role="alert">
                      Import details unavailable.
                    </p>
                  )}
                  <div className="continuation-form">
                    <span className="eyebrow">New session</span>
                    <div className="model-selection">
                      <select
                        aria-label="Initial model selection kind"
                        value={modelKind}
                        disabled={hasRetainedCommand}
                        onChange={(event) => {
                          if (!hasRetainedCommand) setModelKind(event.target.value as ModelKind)
                        }}
                      >
                        <option value="direct">Direct model</option>
                        <option value="alias">Model alias</option>
                      </select>
                      <input
                        aria-label="Initial model selection UUID"
                        placeholder="Model ID"
                        value={modelSelectionId}
                        disabled={hasRetainedCommand}
                        onChange={(event) => {
                          if (!hasRetainedCommand) setModelSelectionId(event.target.value)
                        }}
                      />
                    </div>
                    <p>Position {selectedFrontier?.position.toLocaleString() ?? '—'}</p>
                    <div className="continuation-actions">
                      <button
                        type="button"
                        onClick={() => invokeCommand('imports.continue.resume', commandContext)}
                        disabled={!canContinueImport}
                      >
                        Resume
                      </button>
                      <button
                        type="button"
                        onClick={() => invokeCommand('imports.continue.fork', commandContext)}
                        disabled={!canContinueImport}
                      >
                        Fork
                      </button>
                      {retainedCommandNeedsAction && pendingCommand && (
                        <>
                          <button
                            type="button"
                            onClick={() => invokeCommand('imports.continue.retry', commandContext)}
                          >
                            Retry
                          </button>
                          <button
                            type="button"
                            onClick={() =>
                              invokeCommand('imports.continue.abandon', commandContext)
                            }
                          >
                            Abandon
                          </button>
                        </>
                      )}
                    </div>
                    {retainedStorageFailed && (
                      <p role="alert">Command storage unavailable. Request not sent.</p>
                    )}
                    {retainedCommandNeedsAction && pendingCommand && (
                      <p role="alert">
                        Outcome unknown. Import {pendingCommand.frontier.imported_conversation_id},
                        position {pendingCommand.frontier.position.toLocaleString()}.
                      </p>
                    )}
                    {continuation.isError && !retryableContinuationFailure && !pendingCommand && (
                      <p role="alert">Request rejected.</p>
                    )}
                    {continuation.data && (
                      <>
                        <p className="continuation-result">
                          Session created: {continuation.data.session_id}
                        </p>
                        <p className="continuation-result">
                          Import {continuation.data.frontier.imported_conversation_id}, position{' '}
                          {continuation.data.frontier.position.toLocaleString()}
                        </p>
                      </>
                    )}
                  </div>
                </div>
                <div className="import-window">
                  <div className="import-window-summary">
                    <span>Entries</span>
                    <small>
                      {windowQuery.isError
                        ? 'Entry window unavailable'
                        : entryWindow
                          ? `${entryWindow.first_position.toLocaleString()}–${entryWindow.last_position.toLocaleString()} · ${entryWindow.items.length} loaded`
                          : selectedImport === null
                            ? 'No import selected'
                            : 'Loading window…'}
                    </small>
                  </div>
                  {windowQuery.isError && (
                    <p className="imports-state" role="alert">
                      Entries unavailable.
                    </p>
                  )}
                  {selectedImport === null && <p className="imports-state">No import selected.</p>}
                  {entryWindow && (
                    <ImportedEntries
                      entries={entryWindow.items}
                      logicalEntryCount={descriptor?.entry_count ?? entryWindow.last_position}
                      selected={selectedFrontier}
                      commandContext={commandContext}
                    />
                  )}
                  <ImportedArtifactView entry={selectedEntry} commandContext={commandContext} />
                </div>
              </div>
            </section>
          )}
        </div>
      </div>
      {presentation === 'standalone' && (
        <OverlaySurfaces
          context={commandContext}
          activeId={scenario ? 'imports' : 'production-imports'}
          importsSurface
          navigationDisabled={hasRetainedCommand}
        />
      )}
    </>
  )
}
