import { useInfiniteQuery, useQuery } from '@tanstack/react-query'
import { useState } from 'react'
import type { WebUsageCallPage } from '../generated/web-contract.mjs'
import { costText, tokenSummary, UsageTable, usageGroupIdentity } from '../SearchUsage'
import { costTotalText, totalCost } from './cost'
import type { SearchUsageSource, UsageFilters } from './model'
import { usageSourceOptions } from './queries'
import './usage.css'

export function UsageSurface() {
  const source = useQuery(usageSourceOptions)
  if (source.isError)
    return (
      <p role="alert">
        Usage could not load.{' '}
        <button type="button" onClick={() => void source.refetch()}>
          Retry
        </button>
      </p>
    )
  if (!source.data) return <p role="status">Loading usage…</p>
  return <UsageContent source={source.data} />
}

export function UsageContent({ source }: { source: SearchUsageSource }) {
  const [filters, setFilters] = useState<UsageFilters>(() => {
    const query = new URLSearchParams(window.location.search)
    return {
      sessionId: query.get('session') ?? undefined,
      turnId: query.get('turn') ?? undefined,
      modelId: query.get('model') ?? undefined,
    }
  })
  const summary = useQuery({
    queryKey: ['search-usage', 'summary', filters],
    queryFn: ({ signal }) => source.usageSummary(filters, signal),
  })
  const calls = useInfiniteQuery({
    queryKey: ['search-usage', 'calls', filters],
    initialPageParam: undefined as WebUsageCallPage['continuation'] | undefined,
    queryFn: ({ pageParam, signal }) =>
      source.usageCalls(
        {
          filters,
          order: 'newest',
          maxItems: source.limits.max_usage_call_page_items,
          after: pageParam,
        },
        signal,
      ),
    getNextPageParam: (page) => page.continuation ?? undefined,
    getPreviousPageParam: () => undefined,
    // Match the existing usage workbench's retained window.
    maxPages: 6,
  })
  const rows = calls.data?.pages.flatMap((page) => page.calls) ?? []
  const change = (patch: Partial<UsageFilters>) => setFilters((value) => ({ ...value, ...patch }))
  return (
    <section className="usage-product" aria-label="Usage">
      <div className="usage-filters">
        {(['sessionId', 'turnId', 'modelId'] as const).map((key) => (
          <label key={key} htmlFor={`usage-${key}`}>
            {{ sessionId: 'Session', turnId: 'Turn', modelId: 'Model' }[key]}
            <select
              id={`usage-${key}`}
              aria-label={{ sessionId: 'Session', turnId: 'Turn', modelId: 'Model' }[key]}
              value={filters[key] ?? ''}
              onChange={(event) => change({ [key]: event.target.value || undefined })}
            >
              <option value="">All</option>
              {[
                ...new Set([
                  filters[key],
                  ...rows.map(
                    (row) =>
                      row[
                        { sessionId: 'session_id', turnId: 'turn_id', modelId: 'model_id' }[key] as
                          | 'session_id'
                          | 'turn_id'
                          | 'model_id'
                      ],
                  ),
                ]),
              ]
                .filter((value): value is string => Boolean(value))
                .map((value) => (
                  <option key={value} value={value}>
                    {value}
                  </option>
                ))}
            </select>
          </label>
        ))}
        {(['fromMicros', 'toMicros'] as const).map((key) => (
          <label key={key}>
            {key === 'fromMicros' ? 'From' : 'Until'}
            <input
              type="datetime-local"
              onChange={(event) =>
                change({
                  [key]: event.target.value
                    ? String(new Date(event.target.value).getTime() * 1000)
                    : undefined,
                })
              }
            />
          </label>
        ))}
        <button
          type="button"
          onClick={() => {
            void summary.refetch()
            void calls.refetch()
          }}
        >
          Refresh
        </button>
      </div>
      {(summary.isPending || calls.isPending) && <p role="status">Loading usage…</p>}
      {(summary.isError || calls.isError) && <p role="alert">Usage could not load. Try Refresh.</p>}
      {summary.data && (
        <>
          <p className="usage-total">
            <strong>{costTotalText(totalCost(summary.data.groups, summary.data.truncated))}</strong>{' '}
            ·{' '}
            {summary.data.groups
              .reduce((sum, group) => sum + BigInt(group.call_count), BigInt(0))
              .toLocaleString()}{' '}
            calls
          </p>
          <details>
            <summary>Model and rate details</summary>
            {summary.data.groups.map((group) => (
              <p key={usageGroupIdentity(group)}>
                {group.model_id} · {group.provenance} · {costText(group.cost)} ·{' '}
                {tokenSummary(group.tokens)}
              </p>
            ))}
          </details>
        </>
      )}
      {calls.data && rows.length === 0 && <p>No calls match these filters.</p>}
      <UsageTable
        calls={rows}
        hasNextPage={calls.hasNextPage && !calls.isFetchingNextPage}
        loadNextPage={() => void calls.fetchNextPage()}
      />
      <details>
        <summary>Session and turn costs in loaded calls</summary>
        <p>These subtotals cover the loaded window. Open a session or turn to see its summary.</p>
        {[...new Set(rows.map((row) => row.session_id))].map((sessionId) => (
          <div key={sessionId}>
            <a href={`/sessions?session=${encodeURIComponent(sessionId)}&workspace=true`}>
              Session {sessionId}
            </a>
            <button type="button" onClick={() => change({ sessionId, turnId: undefined })}>
              Show session usage
            </button>
            <p>
              {costTotalText(
                totalCost(
                  rows.filter((row) => row.session_id === sessionId),
                  true,
                ),
              )}
            </p>
            {[
              ...new Set(
                rows.filter((row) => row.session_id === sessionId).map((row) => row.turn_id),
              ),
            ].map((turnId) => (
              <p key={turnId ?? 'session'}>
                {turnId ? (
                  <button type="button" onClick={() => change({ sessionId, turnId })}>
                    Turn {turnId}
                  </button>
                ) : (
                  'Outside a turn'
                )}{' '}
                ·{' '}
                {costTotalText(
                  totalCost(
                    rows.filter((row) => row.session_id === sessionId && row.turn_id === turnId),
                    true,
                  ),
                )}
              </p>
            ))}
          </div>
        ))}
      </details>
    </section>
  )
}
