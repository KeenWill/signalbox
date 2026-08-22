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
import { type ImportApi, ImportApiError, ImportReceiptCorrelationError } from './api'
import { ImportedEntries } from './ImportedEntries'
import { ImportsTable } from './ImportsTable'
import { loadRetainedCommand, storeRetainedCommand } from './retainedCommand'
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
  presentation?: 'standalone' | 'product'
  onCommandContext?: (context: CommandContext | null) => void
  onNavigationDisabledChange?: (disabled: boolean) => void
}) {
  const queryClient = useQueryClient()
  const density = useAppSelector(selectApp).density
  const queryScope = scenario ? 'scenario' : 'production'
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
  const [pendingCommand, setPendingCommand] = useState<WebImportContinuationRequest | null>(() =>
    loadRetainedCommand(queryScope),
  )
  const hasRetainedCommand = pendingCommand !== null

  useEffect(() => {
    onNavigationDisabledChange?.(hasRetainedCommand)
    return () => onNavigationDisabledChange?.(false)
  }, [hasRetainedCommand, onNavigationDisabledChange])

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
    enabled: selectedImport !== null,
  })
  const windowQuery = useQuery({
    queryKey: ['imports', queryScope, selectedImport, 'entries', windowRequest],
    queryFn: ({ signal }) => api.entries(selectedImport ?? '', windowRequest, signal),
    enabled: selectedImport !== null,
  })
  const entryWindow = windowQuery.data
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
      focusTimeline: () =>
        document.querySelector<HTMLElement>('[aria-label="Imported source entries"]')?.focus(),
      importEntryIds,
      selectedImportEntry: selectedFrontier?.imported_entry_id ?? null,
      selectImportEntry,
    }),
    [importEntryIds, selectImportEntry, selectedFrontier?.imported_entry_id],
  )
  useEffect(() => {
    onCommandContext?.(commandContext)
    return () => onCommandContext?.(null)
  }, [commandContext, onCommandContext])
  useHotkeys(
    [...(presentation === 'standalone' ? surfaceHotkeyBindings : []), ...importHotkeyBindings].map(
      (binding) => ({
        hotkey: binding.hotkey,
        callback: () => invokeCommand(binding.commandId, commandContext),
      }),
    ),
  )
  useHotkeySequences(
    [
      ...(presentation === 'standalone' ? surfaceHotkeySequenceBindings : []),
      ...importHotkeySequenceBindings,
    ].map((binding) => ({
      sequence: binding.sequence,
      callback: () => invokeCommand(binding.commandId, commandContext),
    })),
  )

  useEffect(() => {
    if (presentation !== 'standalone') return
    const snapshot: DiagnosticSnapshot = {
      scenario: scenario ? 'imports' : 'production-imports',
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
    presentation,
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
    const position = Number(positionInput)
    if (
      !Number.isSafeInteger(position) ||
      position <= 0 ||
      position > (descriptorQuery.data?.entry_count ?? 0)
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
  const modelSelectionMissing = modelSelectionId.trim().length === 0

  const continueAt = (relationship: WebImportedSessionRelationship) => {
    if (hasRetainedCommand || !selectedFrontier || modelSelectionId.trim().length === 0) return
    const request: WebImportContinuationRequest = {
      command_id: crypto.randomUUID(),
      frontier: selectedFrontier,
      relationship,
      initial_model_selection:
        modelKind === 'direct'
          ? { kind: 'direct', selection_id: modelSelectionId.trim() }
          : { kind: 'alias', alias_id: modelSelectionId.trim() },
    }
    storeRetainedCommand(queryScope, request)
    setPendingCommand(request)
    continuation.mutate(request)
  }

  return (
    <>
      <div className={`imports-shell imports-shell-${presentation}`}>
        {presentation === 'standalone' && (
          <aside className="navigation-pane imports-navigation">
            <ScenarioNavigation
              activeId={scenario ? 'imports' : 'production-imports'}
              disabled={hasRetainedCommand}
            />
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
                    aria-label="Filter imports by exact source session evidence"
                    value={sourceSession}
                    placeholder="Exact attested identifier"
                    disabled={hasRetainedCommand}
                    onChange={(event) => {
                      if (hasRetainedCommand) return
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
                density={density}
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
                          : 'Not attested'}
                      </dd>
                    </div>
                    <div>
                      <dt>Source digest</dt>
                      <dd>{descriptorQuery.data.source.source_digest_sha256}</dd>
                    </div>
                    <div>
                      <dt>Raw records</dt>
                      <dd>{descriptorQuery.data.raw_record_count.toLocaleString()}</dd>
                    </div>
                    <div>
                      <dt>Entries</dt>
                      <dd>{descriptorQuery.data.entry_count.toLocaleString()}</dd>
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
                      <dd>1–{descriptorQuery.data.timeline.latest.position.toLocaleString()}</dd>
                    </div>
                  </dl>
                )}
                {descriptorQuery.isError && (
                  <p className="imports-state" role="alert">
                    The selected import descriptor could not be loaded.
                  </p>
                )}
                <div className="continuation-form">
                  <span className="eyebrow">Create native session from selected frontier</span>
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
                      placeholder="Model selection UUID"
                      value={modelSelectionId}
                      disabled={hasRetainedCommand}
                      onChange={(event) => {
                        if (!hasRetainedCommand) setModelSelectionId(event.target.value)
                      }}
                    />
                  </div>
                  <p>
                    Frontier {selectedFrontier?.position.toLocaleString() ?? '—'} · provider
                    defaults
                  </p>
                  <div className="continuation-actions">
                    <button
                      type="button"
                      onClick={() => continueAt('resume')}
                      disabled={
                        !selectedFrontier ||
                        modelSelectionMissing ||
                        descriptorQuery.isError ||
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
                        !selectedFrontier ||
                        modelSelectionMissing ||
                        descriptorQuery.isError ||
                        continuation.isPending ||
                        hasRetainedCommand
                      }
                    >
                      Fork
                    </button>
                    {retainedCommandNeedsAction && pendingCommand && (
                      <>
                        <button type="button" onClick={() => continuation.mutate(pendingCommand)}>
                          Retry exact command
                        </button>
                        <button
                          type="button"
                          onClick={() => {
                            storeRetainedCommand(queryScope, null)
                            setPendingCommand(null)
                            continuation.reset()
                          }}
                        >
                          Abandon exact retry
                        </button>
                      </>
                    )}
                  </div>
                  {retainedCommandNeedsAction && pendingCommand && (
                    <p role="alert">
                      The exact command for import{' '}
                      {pendingCommand.frontier.imported_conversation_id}, position{' '}
                      {pendingCommand.frontier.position.toLocaleString()}, is retained for retry.
                      Abandon it before selecting another import or frontier.
                    </p>
                  )}
                  {continuation.isError && !retryableContinuationFailure && !pendingCommand && (
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
                        ? `${entryWindow.first_position.toLocaleString()}–${entryWindow.last_position.toLocaleString()} · ${entryWindow.items.length} loaded`
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
                    logicalEntryCount={
                      descriptorQuery.data?.entry_count ?? entryWindow.last_position
                    }
                    selected={selectedFrontier}
                    commandContext={commandContext}
                    density={density}
                  />
                )}
              </div>
            </div>
          </section>
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
