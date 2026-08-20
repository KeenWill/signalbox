import { useHotkeySequences, useHotkeys } from '@tanstack/react-hotkeys'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
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
import { type DiagnosticSnapshot, OverlaySurfaces } from '../Surfaces'
import { store } from '../state'
import type { ImportApi } from './api'
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

const byteLabel = (bytes: number): string => {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`
}

export function ImportsWorkspace({ api, scenario }: { api: ImportApi; scenario: boolean }) {
  const queryClient = useQueryClient()
  const [format, setFormat] = useState<FormatFilter>(EMPTY_FILTER)
  const [sourceSession, setSourceSession] = useState('')
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
  const [pendingCommand, setPendingCommand] = useState<WebImportContinuationRequest | null>(null)

  const listRequest = useMemo(
    () => ({
      after,
      limit: IMPORT_PAGE_ITEMS,
      format: format || undefined,
      source_session_id: sourceSession.trim() || undefined,
    }),
    [after, format, sourceSession],
  )
  const importsQuery = useQuery({
    queryKey: ['imports', 'catalog', listRequest],
    queryFn: () => api.list(listRequest),
  })
  const imports = importsQuery.data
  const firstImport = imports?.items[0]?.imported_conversation_id ?? null

  useEffect(() => {
    if (
      !selectedImport ||
      !imports?.items.some((item) => item.imported_conversation_id === selectedImport)
    ) {
      setSelectedImport(firstImport)
    }
  }, [firstImport, imports?.items, selectedImport])

  const descriptorQuery = useQuery({
    queryKey: ['imports', selectedImport, 'descriptor'],
    queryFn: () => api.descriptor(selectedImport ?? ''),
    enabled: selectedImport !== null,
  })
  const windowQuery = useQuery({
    queryKey: ['imports', selectedImport, 'entries', windowRequest],
    queryFn: () => api.entries(selectedImport ?? '', windowRequest),
    enabled: selectedImport !== null,
  })
  const entryWindow = windowQuery.data
  const firstFrontier = entryWindow?.items[0]?.frontier ?? null
  const importEntryIds = useMemo(
    () => entryWindow?.items.map((entry) => entry.frontier.imported_entry_id) ?? [],
    [entryWindow?.items],
  )

  useEffect(() => {
    setSelectedFrontier(firstFrontier)
  }, [firstFrontier])

  const continuation = useMutation({
    mutationFn: (request: WebImportContinuationRequest) =>
      api.continueImport(request.frontier.imported_conversation_id, request),
    onSuccess: () => setPendingCommand(null),
  })
  const selectImportEntry = useCallback(
    (id: string) => {
      setSelectedFrontier(
        entryWindow?.items.find((entry) => entry.frontier.imported_entry_id === id)?.frontier ??
          null,
      )
    },
    [entryWindow?.items],
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
  useHotkeys(
    [...surfaceHotkeyBindings, ...importHotkeyBindings].map((binding) => ({
      hotkey: binding.hotkey,
      callback: () => invokeCommand(binding.commandId, commandContext),
    })),
  )
  useHotkeySequences(
    [...surfaceHotkeySequenceBindings, ...importHotkeySequenceBindings].map((binding) => ({
      sequence: binding.sequence,
      callback: () => invokeCommand(binding.commandId, commandContext),
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
    setAfter(undefined)
    setSelectedImport(null)
  }

  const showWindow = (request: WebImportEntryWindowRequest) => {
    setWindowRequest(request)
    setSelectedFrontier(null)
  }

  const showPosition = () => {
    const position = Number(positionInput)
    if (!Number.isSafeInteger(position) || position <= 0) return
    showWindow({
      anchor: 'position',
      position,
      before: Math.floor(IMPORT_WINDOW_RADIUS / 2),
      after: Math.floor(IMPORT_WINDOW_RADIUS / 2),
    })
  }

  const continueAt = (relationship: WebImportedSessionRelationship) => {
    if (!selectedFrontier || modelSelectionId.trim().length === 0) return
    const request: WebImportContinuationRequest = {
      command_id: crypto.randomUUID(),
      frontier: selectedFrontier,
      relationship,
      initial_model_selection:
        modelKind === 'direct'
          ? { kind: 'direct', selection_id: modelSelectionId.trim() }
          : { kind: 'alias', alias_id: modelSelectionId.trim() },
    }
    setPendingCommand(request)
    continuation.mutate(request)
  }

  return (
    <>
      <div className="imports-shell">
        <aside className="navigation-pane imports-navigation">
          <ScenarioNavigation activeId="imports" />
        </aside>
        <main className="imports-workspace">
          <header className="imports-header">
            <div>
              <span className="eyebrow">Immutable imported evidence</span>
              <h1>Imports</h1>
            </div>
            <span className="window-count">100-row keyset pages · 101-entry windows</span>
          </header>
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
                    placeholder="Exact attested identifier"
                    onChange={(event) => {
                      setSourceSession(event.target.value)
                      resetCatalog()
                    }}
                  />
                </label>
                <button type="button" disabled={!after} onClick={() => setAfter(undefined)}>
                  First page
                </button>
                <button
                  type="button"
                  disabled={!imports?.next_cursor}
                  onClick={() => setAfter(imports?.next_cursor ?? undefined)}
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
                onSelect={setSelectedImport}
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
                      <dd>{descriptorQuery.data.source.source_session_id ?? 'Not attested'}</dd>
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
                <div className="continuation-form">
                  <span className="eyebrow">Create native session from selected frontier</span>
                  <div className="model-selection">
                    <select
                      aria-label="Initial model selection kind"
                      value={modelKind}
                      onChange={(event) => setModelKind(event.target.value as ModelKind)}
                    >
                      <option value="direct">Direct model</option>
                      <option value="alias">Model alias</option>
                    </select>
                    <input
                      aria-label="Initial model selection UUID"
                      placeholder="Model selection UUID"
                      value={modelSelectionId}
                      onChange={(event) => setModelSelectionId(event.target.value)}
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
                      disabled={!selectedFrontier || continuation.isPending}
                    >
                      Resume
                    </button>
                    <button
                      type="button"
                      onClick={() => continueAt('fork')}
                      disabled={!selectedFrontier || continuation.isPending}
                    >
                      Fork
                    </button>
                    {continuation.isError && pendingCommand && (
                      <button type="button" onClick={() => continuation.mutate(pendingCommand)}>
                        Retry exact command
                      </button>
                    )}
                  </div>
                  {continuation.isError && (
                    <p role="alert">
                      The command identity and payload are retained for an exact retry.
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
                    {entryWindow
                      ? `${entryWindow.first_position.toLocaleString()}–${entryWindow.last_position.toLocaleString()} · ${entryWindow.items.length} loaded`
                      : 'Loading window…'}
                  </small>
                </div>
                {entryWindow && (
                  <ImportedEntries
                    entries={entryWindow.items}
                    logicalEntryCount={
                      descriptorQuery.data?.entry_count ?? entryWindow.last_position
                    }
                    selected={selectedFrontier}
                    commandContext={commandContext}
                  />
                )}
              </div>
            </div>
          </section>
        </main>
      </div>
      <OverlaySurfaces context={commandContext} activeId="imports" />
    </>
  )
}
