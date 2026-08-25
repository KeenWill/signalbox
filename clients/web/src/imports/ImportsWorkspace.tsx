import { useHotkeySequences, useHotkeys } from '@tanstack/react-hotkeys'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Menu } from 'lucide-react'
import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  type CommandContext,
  importHotkeyBindings,
  importHotkeySequenceBindings,
  invokeCommand,
  surfaceHotkeyBindings,
  surfaceHotkeySequenceBindings,
} from '../commands'
import {
  decodeWebImportContinuationRequest,
  type WebImportContinuationReference,
  type WebImportContinuationRequest,
  type WebImportEntryWindowRequest,
  type WebImportedSessionRelationship,
  type WebImportFormat,
} from '../generated/web-contract.mjs'
import { ScenarioNavigation } from '../ScenarioNavigation'
import { type DiagnosticSnapshot, IconCommand, OverlaySurfaces } from '../Surfaces'
import { store } from '../state'
import {
  correlateEntryWindowWithDescriptor,
  type ImportApi,
  ImportApiError,
  ImportReceiptCorrelationError,
} from './api'
import { ImportedArtifactView } from './ImportedArtifactView'
import { ImportedEntries } from './ImportedEntries'
import { ImportsTable } from './ImportsTable'
import { SCENARIO_IMPORT_TOTAL } from './scenario'

const IMPORT_PAGE_ITEMS = 100
const IMPORT_WINDOW_RADIUS = 50
const EMPTY_FILTER = ''
const SCENARIO_MODEL_SELECTION = '00000000-0000-7000-8000-000000000777'

type FormatFilter = WebImportFormat | typeof EMPTY_FILTER
type ModelKind = 'direct' | 'alias'

const formatOptions: ReadonlyArray<{ value: FormatFilter; label: string }> = [
  { value: EMPTY_FILTER, label: 'All source formats' },
  { value: 'claude_code_session_jsonl_v2', label: 'Claude Code · converter 2' },
  { value: 'claude_code_session_jsonl_v1', label: 'Claude Code · converter 1' },
  { value: 'codex_rollout_jsonl_v1', label: 'Codex rollout · converter 1' },
]

const isRetryableContinuationError = (error: unknown): boolean =>
  error instanceof ImportReceiptCorrelationError ||
  !(error instanceof ImportApiError) ||
  ['continuation_commit_ambiguous', 'continuation_unavailable'].includes(error.detail.error.code)

const isAmbiguousContinuationError = (error: unknown): boolean =>
  error instanceof ImportReceiptCorrelationError ||
  !(error instanceof ImportApiError) ||
  error.detail.error.code === 'continuation_commit_ambiguous'

// The exact command must outlive this tab: once its POST leaves the browser, the daemon may
// have committed it, so the only safe replacement source after a reload is a durable copy of
// the same payload retained until a correlated outcome is decoded. Each command owns its own
// slot keyed by its durable identity, so concurrent tabs in the same scope can neither
// overwrite each other's unresolved command nor delete it when their own command settles.
const retainedCommandStoragePrefix = (scope: string): string =>
  `signalbox.imports.retained-continuation.${scope}.`

const retainedCommandStorageKey = (scope: string, commandId: string): string =>
  `${retainedCommandStoragePrefix(scope)}${commandId}`

const readRetainedCommand = (scope: string): WebImportContinuationRequest | null => {
  try {
    const prefix = retainedCommandStoragePrefix(scope)
    const keys: string[] = []
    for (let index = 0; index < window.localStorage.length; index += 1) {
      const key = window.localStorage.key(index)
      if (key?.startsWith(prefix)) keys.push(key)
    }
    for (const key of keys.sort()) {
      const stored = window.localStorage.getItem(key)
      if (stored === null) continue
      try {
        return decodeWebImportContinuationRequest(JSON.parse(stored))
      } catch {
        // An undecodable slot is not a retryable exact command; leave it for inspection.
      }
    }
    return null
  } catch {
    return null
  }
}

// Returns whether the exact payload is durably retained; the caller must not send a
// continuation whose only copy is component state that a reload would destroy.
const persistRetainedCommand = (scope: string, command: WebImportContinuationRequest): boolean => {
  const encoded = JSON.stringify(command)
  const key = retainedCommandStorageKey(scope, command.command_id)
  try {
    window.localStorage.setItem(key, encoded)
    return window.localStorage.getItem(key) === encoded
  } catch {
    return false
  }
}

const clearRetainedCommand = (scope: string, commandId: string): void => {
  try {
    window.localStorage.removeItem(retainedCommandStorageKey(scope, commandId))
  } catch {
    // A failed removal only re-offers an already-settled exact command later.
  }
}

const decimalLabel = (value: string): string => BigInt(value).toLocaleString()

const byteLabel = (value: string): string => {
  const bytes = BigInt(value)
  if (bytes < 1024n) return `${bytes.toLocaleString()} B`
  const unit = bytes < 1024n * 1024n ? 1024n : 1024n * 1024n
  const label = unit === 1024n ? 'KiB' : 'MiB'
  const tenths = (bytes * 10n) / unit
  return `${tenths / 10n}.${tenths % 10n} ${label}`
}

export function ImportsWorkspace({
  api,
  scenario,
  continuationAvailable = true,
  active = true,
  presentation = 'standalone',
  onCommandContext,
}: {
  api: ImportApi
  scenario: boolean
  continuationAvailable?: boolean
  active?: boolean
  presentation?: 'standalone' | 'product'
  onCommandContext?: (context: CommandContext | null) => void
}) {
  const queryClient = useQueryClient()
  const [format, setFormat] = useState<FormatFilter>(EMPTY_FILTER)
  const [sourceSession, setSourceSession] = useState('')
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
  const [modelKind, setModelKind] = useState<ModelKind>('direct')
  const [modelSelectionId, setModelSelectionId] = useState(scenario ? SCENARIO_MODEL_SELECTION : '')
  const queryScope = scenario ? 'scenario' : 'production'
  const [pendingCommand, setPendingCommand] = useState<WebImportContinuationRequest | null>(() =>
    readRetainedCommand(queryScope),
  )
  // A command restored from browser persistence has an unknown durable outcome, which is
  // exactly the ambiguous posture: retry the exact payload until an outcome correlates.
  const [continuationAmbiguous, setContinuationAmbiguous] = useState(pendingCommand !== null)
  const [retentionFailed, setRetentionFailed] = useState(false)
  const hasRetainedCommand = pendingCommand !== null
  const retainCommand = useCallback(
    (command: WebImportContinuationRequest): boolean => {
      if (!persistRetainedCommand(queryScope, command)) return false
      setPendingCommand(command)
      return true
    },
    [queryScope],
  )
  // Settling one command drains the next unresolved slot instead of unlocking new
  // continuations: a browser restart may have left several tabs' ambiguous commands in
  // storage, and each hidden one may already have committed a session.
  const releaseRetainedCommand = useCallback(
    (settled: WebImportContinuationRequest) => {
      clearRetainedCommand(queryScope, settled.command_id)
      const remaining = readRetainedCommand(queryScope)
      setPendingCommand(remaining)
      setContinuationAmbiguous(remaining !== null)
    },
    [queryScope],
  )

  const listRequest = useMemo(
    () => ({
      after,
      limit: IMPORT_PAGE_ITEMS,
      format: format || undefined,
      source_session_id: sourceSessionFilterEnabled ? sourceSession : undefined,
    }),
    [after, format, sourceSession, sourceSessionFilterEnabled],
  )
  const importsQuery = useQuery({
    queryKey: ['imports', queryScope, 'catalog', listRequest],
    queryFn: ({ signal }) => api.list(listRequest, signal),
    enabled: active,
    gcTime: 0,
  })
  const imports = importsQuery.data
  const firstImport = imports?.items[0]?.imported_conversation_id ?? null

  useEffect(() => {
    if (
      !hasRetainedCommand &&
      (!selectedImport ||
        !imports?.items.some((item) => item.imported_conversation_id === selectedImport))
    ) {
      setSelectedImport(firstImport)
      setWindowRequest({ anchor: 'first', before: 0, after: IMPORT_WINDOW_RADIUS })
      setSelectedFrontier(null)
      setPositionInput('')
    }
  }, [firstImport, hasRetainedCommand, imports?.items, selectedImport])

  const descriptorQuery = useQuery({
    queryKey: ['imports', queryScope, selectedImport, 'descriptor'],
    queryFn: ({ signal }) => api.descriptor(selectedImport ?? '', signal),
    enabled: active && selectedImport !== null,
    gcTime: 0,
  })
  const windowQuery = useQuery({
    queryKey: ['imports', queryScope, selectedImport, 'entries', windowRequest],
    queryFn: async ({ signal }) => {
      const importedConversationId = selectedImport ?? ''
      const [descriptor, window] = await Promise.all([
        api.descriptor(importedConversationId, signal),
        api.entries(importedConversationId, windowRequest, signal),
      ])
      return correlateEntryWindowWithDescriptor(windowRequest, window, descriptor)
    },
    enabled: active && selectedImport !== null,
    gcTime: 0,
  })
  const entryWindow = windowQuery.data
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

  useEffect(() => {
    if (!hasRetainedCommand) setSelectedFrontier(anchorFrontier)
  }, [anchorFrontier, hasRetainedCommand])

  const continuation = useMutation({
    mutationFn: (request: WebImportContinuationRequest) =>
      api.continueImport(request.frontier.imported_conversation_id, request),
    onSuccess: (_receipt, request) => {
      releaseRetainedCommand(request)
    },
    onError: (error, request) => {
      const ambiguous = continuationAmbiguous || isAmbiguousContinuationError(error)
      if (ambiguous) setContinuationAmbiguous(true)
      if (!ambiguous && !isRetryableContinuationError(error)) {
        releaseRetainedCommand(request)
      }
    },
  })
  const resetContinuation = continuation.reset
  const selectImportEntry = useCallback(
    (id: string) => {
      if (hasRetainedCommand) return
      resetContinuation()
      setSelectedFrontier(
        entryWindow?.items.find((entry) => entry.frontier.imported_entry_id === id)?.frontier ??
          null,
      )
    },
    [entryWindow?.items, hasRetainedCommand, resetContinuation],
  )
  const commandContext = useMemo<CommandContext>(
    () => ({
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds:
        selectedEntry?.content_kind === 'text' ? [selectedEntry.frontier.imported_entry_id] : [],
      artifactOriginalIds: [],
      focusTimeline: () =>
        document.querySelector<HTMLElement>('[aria-label="Imported source entries"]')?.focus(),
      importEntryIds,
      selectedImportEntry: selectedFrontier?.imported_entry_id ?? null,
      selectImportEntry,
    }),
    [importEntryIds, selectImportEntry, selectedEntry, selectedFrontier?.imported_entry_id],
  )
  useEffect(() => {
    onCommandContext?.(commandContext)
    return () => onCommandContext?.(null)
  }, [commandContext, onCommandContext])
  useHotkeys(
    [...(presentation === 'standalone' ? surfaceHotkeyBindings : []), ...importHotkeyBindings].map(
      (binding) => ({
        hotkey: binding.hotkey,
        callback: () => {
          if (active && store.getState().app.overlay === null) {
            invokeCommand(binding.commandId, commandContext)
          }
        },
      }),
    ),
  )
  useHotkeySequences(
    [
      ...(presentation === 'standalone' ? surfaceHotkeySequenceBindings : []),
      ...importHotkeySequenceBindings,
    ].map((binding) => ({
      sequence: binding.sequence,
      callback: () => {
        if (active && store.getState().app.overlay === null) {
          invokeCommand(binding.commandId, commandContext)
        }
      },
    })),
  )

  useEffect(() => {
    const snapshot: DiagnosticSnapshot = {
      scenario: scenario ? 'imports' : 'production-imports',
      connection: importsQuery.isError || windowQuery.isError ? 'failed' : 'ready',
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
    resetContinuation()
    setWindowRequest(request)
    setSelectedFrontier(null)
  }

  const showPosition = () => {
    if (!/^[1-9]\d{0,19}$/.test(positionInput)) return
    const position = BigInt(positionInput)
    const entryCount = descriptorQuery.data?.entry_count
    if (entryCount === undefined || position > BigInt(entryCount)) return
    showWindow({
      anchor: 'position',
      position: positionInput,
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
  // A retained command is offered for exact retry after a retryable failure and after a
  // reload restored it from browser persistence with its durable outcome still unknown.
  const commandRetainedForRetry =
    pendingCommand !== null &&
    !continuation.isPending &&
    (continuation.isError ? retryableContinuationFailure : continuationAmbiguous)
  const ambiguousContinuationFailure = commandRetainedForRetry && continuationAmbiguous

  const continueAt = (relationship: WebImportedSessionRelationship) => {
    if (
      !continuationAvailable ||
      !selectedFrontier ||
      modelSelectionId.trim().length === 0 ||
      hasRetainedCommand
    ) {
      return
    }
    const request: WebImportContinuationRequest = {
      command_id: crypto.randomUUID(),
      frontier: selectedFrontier,
      relationship,
      initial_model_selection:
        modelKind === 'direct'
          ? { kind: 'direct', selection_id: modelSelectionId.trim() }
          : { kind: 'alias', alias_id: modelSelectionId.trim() },
    }
    // The POST leaves the browser only after the exact payload is durably retained: sending
    // first would let a reload destroy the sole copy of a command the daemon may commit.
    if (!retainCommand(request)) {
      setRetentionFailed(true)
      return
    }
    setRetentionFailed(false)
    setContinuationAmbiguous(false)
    continuation.mutate(request)
  }

  return (
    <>
      <div className={`imports-shell imports-shell-${presentation}`}>
        {presentation === 'standalone' && (
          <aside className="navigation-pane imports-navigation">
            <ScenarioNavigation activeId="imports" />
          </aside>
        )}
        <div
          className={`imports-workspace imports-workspace-${presentation}`}
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
                <span className="eyebrow">Immutable imported evidence</span>
                <h1>Imports</h1>
              </div>
              <span className="window-count">100-row keyset pages · 101-entry windows</span>
            </header>
          )}
          <section className="imports-catalog" aria-labelledby="imports-catalog-heading">
            <header className="section-header imports-catalog-header">
              <div>
                <span className="eyebrow">Bounded discovery</span>
                <h2 id="imports-catalog-heading">Imported conversations</h2>
              </div>
              <div className="imports-filters">
                <label>
                  <span>Format</span>
                  <select
                    aria-label="Filter imports by format"
                    value={format}
                    disabled={hasRetainedCommand}
                    onChange={(event) => {
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
                    aria-label="Filter imports by exact source session evidence"
                    value={sourceSession}
                    disabled={hasRetainedCommand}
                    placeholder="Exact attested identifier"
                    onChange={(event) => {
                      setSourceSession(event.target.value)
                      resetCatalog()
                    }}
                  />
                </label>
                <label className="source-session-filter-toggle">
                  <span>Exact filter</span>
                  <input
                    aria-label="Use exact source session filter"
                    type="checkbox"
                    checked={sourceSessionFilterEnabled}
                    disabled={hasRetainedCommand}
                    onChange={(event) => {
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
                  First page
                </button>
                <button
                  type="button"
                  disabled={!imports?.next_cursor || hasRetainedCommand}
                  onClick={() => showCatalogPage(imports?.next_cursor ?? undefined)}
                >
                  Next page
                </button>
              </div>
            </header>
            {importsQuery.isPending && <p className="imports-state">Loading bounded imports…</p>}
            {importsQuery.isError && (
              <p className="imports-state" role="alert">
                Imported-conversation discovery is unavailable.
              </p>
            )}
            {imports && (
              <ImportsTable
                rows={imports.items}
                selectedId={selectedImport}
                onSelect={selectImport}
              />
            )}
          </section>
          <section className="import-inspector" aria-labelledby="import-inspector-heading">
            <header className="section-header import-inspector-header">
              <div>
                <span className="eyebrow">Descriptor and selected source frontier</span>
                <h2 id="import-inspector-heading">
                  {descriptorQuery.data?.display_title ?? 'Import inspector'}
                </h2>
              </div>
              <div className="window-controls">
                <button
                  type="button"
                  onClick={() =>
                    showWindow({ anchor: 'first', before: 0, after: IMPORT_WINDOW_RADIUS })
                  }
                >
                  First
                </button>
                <button
                  type="button"
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
                    onChange={(event) => setPositionInput(event.target.value)}
                  />
                </label>
                <button type="button" onClick={showPosition}>
                  Go
                </button>
              </div>
            </header>
            <div className="import-inspector-body">
              <div className="import-evidence">
                {descriptorQuery.data && (
                  <dl>
                    <div>
                      <dt>Import identity</dt>
                      <dd>{descriptorQuery.data.imported_conversation_id}</dd>
                    </div>
                    <div>
                      <dt>Format</dt>
                      <dd>{descriptorQuery.data.source.format}</dd>
                    </div>
                    <div>
                      <dt>Source session</dt>
                      <dd>
                        {descriptorQuery.data.source.source_session_id
                          ? `${descriptorQuery.data.source.source_session_id.leading_text}${
                              descriptorQuery.data.source.source_session_id.completeness ===
                              'truncated'
                                ? '…'
                                : ''
                            }`
                          : 'Unknown or inconsistent source-session evidence'}
                      </dd>
                    </div>
                    <div>
                      <dt>Source digest</dt>
                      <dd>{descriptorQuery.data.source.source_digest_sha256}</dd>
                    </div>
                    <div>
                      <dt>Raw records</dt>
                      <dd>{decimalLabel(descriptorQuery.data.raw_record_count)}</dd>
                    </div>
                    <div>
                      <dt>Entries</dt>
                      <dd>{decimalLabel(descriptorQuery.data.entry_count)}</dd>
                    </div>
                    <div>
                      <dt>Raw source size</dt>
                      <dd>{byteLabel(descriptorQuery.data.sizes.raw_source_bytes)}</dd>
                    </div>
                    <div>
                      <dt>Normalized records</dt>
                      <dd>
                        {byteLabel(descriptorQuery.data.sizes.normalized_source_record_bytes)}
                      </dd>
                    </div>
                    <div>
                      <dt>Normalized entries</dt>
                      <dd>{byteLabel(descriptorQuery.data.sizes.normalized_entry_bytes)}</dd>
                    </div>
                    <div>
                      <dt>Timeline</dt>
                      <dd>1–{decimalLabel(descriptorQuery.data.timeline.latest.position)}</dd>
                    </div>
                  </dl>
                )}
                <div className="continuation-form">
                  <span className="eyebrow">Create native session from selected frontier</span>
                  <div className="model-selection">
                    <select
                      aria-label="Initial model selection kind"
                      value={modelKind}
                      disabled={hasRetainedCommand}
                      onChange={(event) => setModelKind(event.target.value as ModelKind)}
                    >
                      <option value="direct">Direct model</option>
                      <option value="alias">Model alias</option>
                    </select>
                    <input
                      aria-label="Initial model selection UUID"
                      placeholder="Model selection UUID"
                      value={modelSelectionId}
                      disabled={hasRetainedCommand}
                      onChange={(event) => setModelSelectionId(event.target.value)}
                    />
                  </div>
                  <p>
                    Frontier {selectedFrontier ? decimalLabel(selectedFrontier.position) : '—'} ·
                    provider defaults
                  </p>
                  <div className="continuation-actions">
                    <button
                      type="button"
                      onClick={() => continueAt('resume')}
                      disabled={
                        !continuationAvailable ||
                        !selectedFrontier ||
                        modelSelectionId.trim().length === 0 ||
                        continuation.isPending ||
                        hasRetainedCommand
                      }
                    >
                      Resume
                    </button>
                    <button
                      type="button"
                      onClick={() => continueAt('fork')}
                      disabled={
                        !continuationAvailable ||
                        !selectedFrontier ||
                        modelSelectionId.trim().length === 0 ||
                        continuation.isPending ||
                        hasRetainedCommand
                      }
                    >
                      Fork
                    </button>
                    {commandRetainedForRetry && pendingCommand && (
                      <>
                        <button type="button" onClick={() => continuation.mutate(pendingCommand)}>
                          Retry exact command
                        </button>
                        {!ambiguousContinuationFailure && (
                          <button
                            type="button"
                            onClick={() => {
                              releaseRetainedCommand(pendingCommand)
                              continuation.reset()
                            }}
                          >
                            Abandon exact retry
                          </button>
                        )}
                      </>
                    )}
                  </div>
                  {!continuationAvailable && (
                    <p role="status">Imported continuation is not advertised by this daemon.</p>
                  )}
                  {retentionFailed && (
                    <p role="alert">
                      The exact command could not be durably retained in this browser, so no
                      continuation was sent. Enable browser storage for this site and retry.
                    </p>
                  )}
                  {commandRetainedForRetry && pendingCommand && (
                    <p role="alert">
                      The exact command for import{' '}
                      {pendingCommand.frontier.imported_conversation_id}, position{' '}
                      {decimalLabel(pendingCommand.frontier.position)}, is retained for retry.
                      {ambiguousContinuationFailure
                        ? ' Retry this exact command until its durable outcome is known.'
                        : ' Abandon it before selecting another import or frontier.'}
                    </p>
                  )}
                  {continuation.isError && !retryableContinuationFailure && (
                    <p role="alert">
                      The continuation request was rejected and cannot be retried unchanged.
                    </p>
                  )}
                  {continuation.data && (
                    <p className="continuation-result">
                      Session created: {continuation.data.session_id}
                    </p>
                  )}
                </div>
              </div>
              <div className="import-window">
                <div className="import-window-summary">
                  <span>Imported source evidence</span>
                  <small>
                    {windowQuery.isError
                      ? 'Entry window unavailable'
                      : entryWindow
                        ? `${decimalLabel(entryWindow.first_position)}–${decimalLabel(entryWindow.last_position)} · ${entryWindow.items.length} loaded`
                        : selectedImport === null
                          ? 'No import selected'
                          : 'Loading window…'}
                  </small>
                </div>
                {windowQuery.isError && (
                  <p className="imports-state" role="alert">
                    The selected imported-entry window could not be loaded.
                  </p>
                )}
                {selectedImport === null && (
                  <p className="imports-state">Select an imported conversation to inspect.</p>
                )}
                {entryWindow && (
                  <ImportedEntries
                    entries={entryWindow.items}
                    logicalEntryCount={descriptorQuery.data?.entry_count}
                    selected={selectedFrontier}
                    commandContext={commandContext}
                  />
                )}
                <ImportedArtifactView entry={selectedEntry} commandContext={commandContext} />
              </div>
            </div>
          </section>
        </div>
      </div>
      {presentation === 'standalone' && (
        <OverlaySurfaces context={commandContext} activeId="imports" />
      )}
    </>
  )
}
