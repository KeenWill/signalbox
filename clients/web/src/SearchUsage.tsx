import { useInfiniteQuery, useQuery } from '@tanstack/react-query'
import { flexRender } from '@tanstack/react-table'
import { getCoreRowModel, type LegacyColumnDef, useLegacyTable } from '@tanstack/react-table/legacy'
import { useVirtualizer } from '@tanstack/react-virtual'
import { Search } from 'lucide-react'
import { useEffect, useMemo, useRef, useState } from 'react'
import type {
  WebSearchPage,
  WebUsageCallKind,
  WebUsageCallPage,
} from './generated/web-contract.mjs'
import type { SearchUsageSource, UsageFilters } from './search-usage/model'

const SEARCH_PAGE_ITEMS = 72
const USAGE_PAGE_ITEMS = 100
// Tunable effective ceilings: repeated "Load next bounded page" evicts the oldest retained page
// instead of accumulating every visited page for the lifetime of the surface, so retained browser
// records stay bounded by pages x per-page items rather than by how long an operator paginates.
const SEARCH_RETAINED_PAGES = 6
const USAGE_RETAINED_PAGES = 6
const SEARCH_OVERSCAN_ROWS = 6
const USAGE_OVERSCAN_ROWS = 8

export interface SearchUsageRouteState {
  view: 'search' | 'usage'
  q: string
  searchScope: 'global' | 'session'
  usageSession: 'all' | 'current'
  provenance?: 'reported' | 'estimated'
  modelId?: string
  callKind?: WebUsageCallKind
}

export const defaultSearchUsageRouteState: SearchUsageRouteState = {
  view: 'search',
  q: '',
  searchScope: 'global',
  usageSession: 'all',
}

type SearchResult = WebSearchPage['results'][number]
type UsageCall = WebUsageCallPage['calls'][number]

const shortIdentity = (value: string): string => `${value.slice(0, 8)}…${value.slice(-4)}`

const tokenText = (value: string | null): string =>
  value === null ? '—' : BigInt(value).toLocaleString()

const tokenSummary = (tokens: UsageCall['tokens']): string =>
  `in ${tokenText(tokens.input)} · out ${tokenText(tokens.output)} · cache ${tokenText(tokens.cache_read_input)}`

const costText = (cost: UsageCall['cost']): string =>
  cost.status === 'derived'
    ? `$${cost.amount_usd} · ${cost.label.replaceAll('_', ' ')} · ${cost.rate_version}`
    : `Unavailable · ${cost.reason.replaceAll('_', ' ')}`

interface SnippetPart {
  text: string
  highlighted: boolean
}

const snippetParts = (result: SearchResult): SnippetPart[] => {
  const encoded = new TextEncoder().encode(result.snippet)
  const decoder = new TextDecoder('utf-8', { fatal: true })
  const parts: SnippetPart[] = []
  let cursor = 0
  for (const highlight of result.highlights) {
    if (highlight.start_byte > cursor) {
      parts.push({
        text: decoder.decode(encoded.slice(cursor, highlight.start_byte)),
        highlighted: false,
      })
    }
    parts.push({
      text: decoder.decode(encoded.slice(highlight.start_byte, highlight.end_byte)),
      highlighted: true,
    })
    cursor = highlight.end_byte
  }
  if (cursor < encoded.length) {
    parts.push({ text: decoder.decode(encoded.slice(cursor)), highlighted: false })
  }
  return parts
}

function SearchResults({
  results,
  selected,
  onSelected,
  onReveal,
  hasNextPage,
  loadNextPage,
}: {
  results: readonly SearchResult[]
  selected: number
  onSelected: (index: number) => void
  onReveal: (result: SearchResult) => void
  hasNextPage: boolean
  loadNextPage: () => void
}) {
  'use no memo'
  const parentRef = useRef<HTMLDivElement>(null)
  const virtualizer = useVirtualizer({
    count: results.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 62,
    overscan: SEARCH_OVERSCAN_ROWS,
    getItemKey: (index) => {
      const result = results[index]
      return result
        ? `${result.session_id}:${result.address.event_sequence}:${result.source.kind}`
        : index
    },
  })
  const virtualRows = virtualizer.getVirtualItems()

  useEffect(() => {
    if (selected >= 0) virtualizer.scrollToIndex(selected, { align: 'auto' })
  }, [selected, virtualizer])

  const onKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (event.key === 'ArrowDown') {
      event.preventDefault()
      onSelected(Math.min(selected + 1, results.length - 1))
    }
    if (event.key === 'ArrowUp') {
      event.preventDefault()
      onSelected(Math.max(selected - 1, 0))
    }
    if (event.key === 'Enter') {
      const result = results[selected]
      if (result) {
        event.preventDefault()
        onReveal(result)
      }
    }
  }

  return (
    <div className="search-results-frame">
      <div
        ref={parentRef}
        className="virtual-scroll search-results"
        role="listbox"
        aria-label="Lexical search results"
        aria-activedescendant={selected >= 0 ? `search-result-${selected}` : undefined}
        tabIndex={0}
        onKeyDown={onKeyDown}
        data-mounted-rows={virtualRows.length}
        data-total-loaded={results.length}
      >
        <div className="virtual-stage" style={{ height: virtualizer.getTotalSize() }}>
          {virtualRows.map((virtualRow) => {
            const result = results[virtualRow.index]
            if (!result) return null
            return (
              // biome-ignore lint/a11y: Focus remains on the aria-activedescendant listbox.
              <div
                id={`search-result-${virtualRow.index}`}
                key={`${result.session_id}:${result.address.event_sequence}:${result.source.kind}`}
                role="option"
                aria-selected={selected === virtualRow.index}
                aria-posinset={virtualRow.index + 1}
                aria-setsize={results.length}
                className="search-result-row"
                data-index={virtualRow.index}
                ref={virtualizer.measureElement}
                style={{ transform: `translateY(${virtualRow.start}px)` }}
                onClick={() => onSelected(virtualRow.index)}
                onDoubleClick={() => onReveal(result)}
              >
                <span className="result-address">@{result.address.event_sequence}</span>
                <span className="result-copy">
                  <strong>{result.content_class.replaceAll('_', ' ')}</strong>
                  <span>
                    {snippetParts(result).map((part, index) =>
                      part.highlighted ? (
                        // biome-ignore lint/suspicious/noArrayIndexKey: Ordered byte spans have no independent identity.
                        <mark key={index}>{part.text}</mark>
                      ) : (
                        // biome-ignore lint/suspicious/noArrayIndexKey: Ordered byte spans have no independent identity.
                        <span key={index}>{part.text}</span>
                      ),
                    )}
                  </span>
                </span>
                <span className="result-owner">{result.source.kind.replaceAll('_', ' ')}</span>
              </div>
            )
          })}
        </div>
      </div>
      {hasNextPage && (
        <button type="button" className="load-more" onClick={loadNextPage}>
          Load next bounded page
        </button>
      )}
    </div>
  )
}

function UsageTable({
  calls,
  hasNextPage,
  loadNextPage,
}: {
  calls: readonly UsageCall[]
  hasNextPage: boolean
  loadNextPage: () => void
}) {
  'use no memo'
  const columns = useMemo<LegacyColumnDef<UsageCall>[]>(
    () => [
      {
        accessorKey: 'recorded_at_micros',
        header: 'Recorded',
        cell: ({ getValue }) => String(getValue()),
      },
      {
        accessorKey: 'model_id',
        header: 'Model',
        cell: ({ getValue }) => shortIdentity(String(getValue())),
      },
      {
        accessorKey: 'provenance',
        header: 'Evidence',
        cell: ({ row }) =>
          `${row.original.provenance} · ${row.original.call_kind.replaceAll('_', ' ')}`,
      },
      { id: 'tokens', header: 'Token axes', cell: ({ row }) => tokenSummary(row.original.tokens) },
      { id: 'cost', header: 'Configured cost', cell: ({ row }) => costText(row.original.cost) },
    ],
    [],
  )
  const table = useLegacyTable({ data: [...calls], columns, getCoreRowModel: getCoreRowModel() })
  const rows = table.getRowModel().rows
  const parentRef = useRef<HTMLDivElement>(null)
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 34,
    overscan: USAGE_OVERSCAN_ROWS,
    getItemKey: (index) => rows[index]?.original.call_id ?? index,
  })
  const virtualRows = virtualizer.getVirtualItems()

  return (
    <div className="usage-table-frame">
      {/* biome-ignore lint/a11y/useSemanticElements: Virtualization requires a scrollable ARIA table container. */}
      <div className="data-table usage-table" role="table" aria-label="Individual model calls">
        {/* biome-ignore lint/a11y: Read-only virtual headers do not receive independent focus. */}
        <div className="table-header" role="row">
          {table.getHeaderGroups()[0]?.headers.map((header) => (
            // biome-ignore lint/a11y: Virtualized ARIA column headers do not receive independent focus.
            <div role="columnheader" key={header.id}>
              {flexRender(header.column.columnDef.header, header.getContext())}
            </div>
          ))}
        </div>
        {/* biome-ignore lint/a11y: A native row group cannot own this keyboard-reachable virtual scroll stage. */}
        <div
          ref={parentRef}
          className="virtual-scroll table-scroll"
          role="rowgroup"
          aria-label="Usage call rows"
          // biome-ignore lint/a11y/noNoninteractiveTabindex: The scroll viewport needs keyboard focus to reveal virtual rows.
          tabIndex={0}
          data-mounted-rows={virtualRows.length}
          data-total-loaded={rows.length}
        >
          <div className="virtual-stage" style={{ height: virtualizer.getTotalSize() }}>
            {virtualRows.map((virtualRow) => {
              const row = rows[virtualRow.index]
              if (!row) return null
              return (
                // biome-ignore lint/a11y: Virtualized ARIA rows are selected through the containing table, not focused.
                <div
                  className="table-row"
                  role="row"
                  aria-rowindex={virtualRow.index + 2}
                  key={row.original.call_id}
                  style={{
                    height: virtualRow.size,
                    transform: `translateY(${virtualRow.start}px)`,
                  }}
                >
                  {row.getVisibleCells().map((cell) => (
                    // biome-ignore lint/a11y/useSemanticElements: Virtualized ARIA cells cannot be native table cells here.
                    <div role="cell" key={cell.id}>
                      {flexRender(cell.column.columnDef.cell, cell.getContext())}
                    </div>
                  ))}
                </div>
              )
            })}
          </div>
        </div>
      </div>
      {hasNextPage && (
        <button type="button" className="load-more" onClick={loadNextPage}>
          Load next call page
        </button>
      )}
    </div>
  )
}

export function SearchUsageWorkbench({
  source,
  currentSessionId,
  route,
  onRouteChange,
  onReveal,
}: {
  source: SearchUsageSource
  currentSessionId: string
  route: SearchUsageRouteState
  onRouteChange: (patch: Partial<SearchUsageRouteState>) => void
  onReveal: (result: SearchResult) => Promise<void>
}) {
  const [draftQuery, setDraftQuery] = useState(route.q)
  const searchIdentity = `${route.searchScope}\u0000${route.q}`
  const [searchSelection, setSearchSelection] = useState({ identity: searchIdentity, index: 0 })
  const selectedSearch = searchSelection.identity === searchIdentity ? searchSelection.index : 0
  const [revealState, setRevealState] = useState<'idle' | 'loading' | 'failed'>('idle')

  useEffect(() => setDraftQuery(route.q), [route.q])

  const searchQuery = useInfiniteQuery({
    queryKey: ['search-usage', 'search', route.q, route.searchScope, currentSessionId],
    enabled: route.view === 'search' && route.q.trim().length > 0,
    initialPageParam: undefined as WebSearchPage['continuation'],
    queryFn: ({ pageParam, signal }) =>
      source.search(
        {
          text: route.q,
          scope:
            route.searchScope === 'session'
              ? { kind: 'session', sessionId: currentSessionId }
              : { kind: 'global' },
          maxItems: SEARCH_PAGE_ITEMS,
          after: pageParam,
        },
        signal,
      ),
    getNextPageParam: (lastPage) => lastPage.continuation ?? undefined,
    // The search contract carries forward continuations only, so no backward page param exists.
    getPreviousPageParam: () => undefined,
    maxPages: SEARCH_RETAINED_PAGES,
  })
  const results = searchQuery.data?.pages.flatMap((page) => page.results) ?? []

  const filters = useMemo<UsageFilters>(
    () => ({
      sessionId: route.usageSession === 'current' ? currentSessionId : undefined,
      modelId: route.modelId,
      provenance: route.provenance,
      callKind: route.callKind,
    }),
    [currentSessionId, route.callKind, route.modelId, route.provenance, route.usageSession],
  )
  const usageSummary = useQuery({
    queryKey: ['search-usage', 'summary', filters],
    enabled: route.view === 'usage',
    queryFn: ({ signal }) => source.usageSummary(filters, signal),
  })
  const usageCalls = useInfiniteQuery({
    queryKey: ['search-usage', 'calls', filters],
    enabled: route.view === 'usage',
    initialPageParam: undefined as WebUsageCallPage['continuation'],
    queryFn: ({ pageParam, signal }) =>
      source.usageCalls(
        { filters, order: 'newest', maxItems: USAGE_PAGE_ITEMS, after: pageParam },
        signal,
      ),
    getNextPageParam: (lastPage) => lastPage.continuation ?? undefined,
    // The usage-call contract carries forward continuations only, so no backward page param exists.
    getPreviousPageParam: () => undefined,
    maxPages: USAGE_RETAINED_PAGES,
  })
  const calls = usageCalls.data?.pages.flatMap((page) => page.calls) ?? []

  const reveal = async (result: SearchResult) => {
    setRevealState('loading')
    try {
      await onReveal(result)
      setRevealState('idle')
    } catch {
      setRevealState('failed')
    }
  }

  return (
    <section className="search-usage-panel" aria-labelledby="search-usage-heading">
      <header className="search-usage-header">
        <div>
          <span className="eyebrow">Dedicated projections</span>
          <h2 id="search-usage-heading">Search and usage</h2>
        </div>
        <div className="surface-tabs" role="tablist" aria-label="Search and usage views">
          <button
            type="button"
            role="tab"
            aria-selected={route.view === 'search'}
            onClick={() => onRouteChange({ view: 'search' })}
          >
            Search
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={route.view === 'usage'}
            onClick={() => onRouteChange({ view: 'usage' })}
          >
            Usage
          </button>
        </div>
      </header>

      {route.view === 'search' ? (
        <div className="search-surface" role="tabpanel">
          <form
            className="lexical-search"
            onSubmit={(event) => {
              event.preventDefault()
              onRouteChange({ q: draftQuery.trim() })
            }}
          >
            <Search aria-hidden="true" />
            <input
              id="lexical-search-input"
              aria-label="Search canonical session evidence"
              placeholder="Search canonical evidence"
              value={draftQuery}
              onChange={(event) => setDraftQuery(event.target.value)}
            />
            <fieldset className="segmented" aria-label="Search scope">
              <button
                type="button"
                aria-pressed={route.searchScope === 'global'}
                onClick={() => onRouteChange({ searchScope: 'global' })}
              >
                Global
              </button>
              <button
                type="button"
                aria-pressed={route.searchScope === 'session'}
                onClick={() => onRouteChange({ searchScope: 'session' })}
              >
                Current session
              </button>
            </fieldset>
            <button type="submit" className="primary-button">
              Run lexical search
            </button>
          </form>
          <div className="surface-status" aria-live="polite">
            <span>{results.length} loaded</span>
            <span>Arrow keys navigate · Enter reveals unloaded context</span>
            {revealState === 'loading' && <span>Loading surrounding timeline window…</span>}
            {revealState === 'failed' && (
              <span role="alert">Match context could not be revealed.</span>
            )}
          </div>
          {searchQuery.isError ? (
            <p className="surface-error" role="alert">
              Search projection could not answer this bounded query.
            </p>
          ) : (
            <SearchResults
              results={results}
              selected={results.length === 0 ? -1 : Math.min(selectedSearch, results.length - 1)}
              onSelected={(index) => setSearchSelection({ identity: searchIdentity, index })}
              onReveal={(result) => void reveal(result)}
              hasNextPage={searchQuery.hasNextPage}
              loadNextPage={() => void searchQuery.fetchNextPage()}
            />
          )}
        </div>
      ) : (
        <div className="usage-surface" role="tabpanel">
          <div className="usage-controls">
            <fieldset className="segmented" aria-label="Usage session scope">
              <button
                type="button"
                aria-pressed={route.usageSession === 'all'}
                onClick={() => onRouteChange({ usageSession: 'all' })}
              >
                All sessions
              </button>
              <button
                type="button"
                aria-pressed={route.usageSession === 'current'}
                onClick={() => onRouteChange({ usageSession: 'current' })}
              >
                Current session
              </button>
            </fieldset>
            {(route.modelId || route.provenance || route.callKind) && (
              <button
                type="button"
                className="secondary-button"
                onClick={() =>
                  onRouteChange({ modelId: undefined, provenance: undefined, callKind: undefined })
                }
              >
                Clear drill-down
              </button>
            )}
          </div>
          <section className="usage-strips" aria-label="Usage summary groups">
            {usageSummary.data?.groups.map((group) => (
              <button
                type="button"
                key={`${group.model_id}:${group.provenance}:${group.call_kind}:${costText(group.cost)}`}
                onClick={() =>
                  onRouteChange({
                    modelId: group.model_id,
                    provenance: group.provenance,
                    callKind: group.call_kind,
                  })
                }
              >
                <span>{shortIdentity(group.model_id)}</span>
                <strong>
                  {group.provenance} · {group.call_count} calls
                </strong>
                <small>{tokenSummary(group.tokens)}</small>
                <small>{costText(group.cost)}</small>
              </button>
            ))}
          </section>
          {usageSummary.data?.truncated && (
            <p className="surface-warning">Summary reached its advertised group ceiling.</p>
          )}
          {usageSummary.isError || usageCalls.isError ? (
            <p className="surface-error" role="alert">
              Usage projection could not answer this bounded query.
            </p>
          ) : (
            <UsageTable
              calls={calls}
              hasNextPage={usageCalls.hasNextPage}
              loadNextPage={() => void usageCalls.fetchNextPage()}
            />
          )}
        </div>
      )}
    </section>
  )
}
