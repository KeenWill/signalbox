import { useInfiniteQuery, useQuery } from '@tanstack/react-query'
import { useLocation, useNavigate } from '@tanstack/react-router'
import { useMemo, useState } from 'react'
import type { WebUsageCallPage } from '../generated/web-contract.mjs'
import { costText, tokenSummary, UsageTable, usageGroupIdentity } from '../SearchUsage'
import { costTotalText, totalCost } from './cost'
import { HttpSearchUsageSource, type SearchUsageSource, type UsageFilters } from './model'
import './usage.css'

export const usageSourceOptions = {
  queryKey: ['usage-http-source'],
  queryFn: () => HttpSearchUsageSource.connect(),
  staleTime: Infinity,
}

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
  return <UsageContent source={source.data} authority="http" />
}

export function UsageContent({
  source,
  authority,
}: {
  source: SearchUsageSource
  authority: 'http' | 'scenario'
}) {
  const search = useLocation({ select: (location) => location.searchStr })
  const navigate = useNavigate()
  const filters = useMemo(() => {
    const query = new URLSearchParams(search)
    return {
      sessionId: query.get('session') || undefined,
      turnId: query.get('turn') || undefined,
      modelId: query.get('model') || undefined,
      fromMicros: query.get('from') || undefined,
      toMicros: query.get('until') || undefined,
    }
  }, [search])
  const [subtotalsOpen, setSubtotalsOpen] = useState(false)
  const summary = useQuery({
    queryKey: ['search-usage', authority, 'summary', filters],
    queryFn: ({ signal }) => source.usageSummary(filters, signal),
  })
  const calls = useInfiniteQuery({
    queryKey: ['search-usage', authority, 'calls', filters],
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
  const rows = useMemo(() => calls.data?.pages.flatMap((page) => page.calls) ?? [], [calls.data])
  const change = (patch: Partial<UsageFilters>) => {
    const query = new URLSearchParams(search)
    for (const [key, parameter] of Object.entries({
      sessionId: 'session',
      turnId: 'turn',
      modelId: 'model',
      fromMicros: 'from',
      toMicros: 'until',
    })) {
      if (!(key in patch)) continue
      const value = patch[key as keyof UsageFilters]
      if (value) query.set(parameter, value)
      else query.delete(parameter)
    }
    void navigate({
      to: '.',
      search: (previous) => ({
        ...previous,
        ...Object.fromEntries(query),
        session: query.get('session') ?? undefined,
        turn: query.get('turn') ?? undefined,
        model: query.get('model') ?? undefined,
        from: query.get('from') ?? undefined,
        until: query.get('until') ?? undefined,
      }),
    })
  }
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
              value={localDateTime(filters[key])}
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
      <details onToggle={(event) => setSubtotalsOpen(event.currentTarget.open)}>
        <summary>Session and turn costs in loaded calls</summary>
        {subtotalsOpen && <LoadedSubtotals rows={rows} change={change} />}
      </details>
    </section>
  )
}

function localDateTime(micros?: string): string {
  if (!micros) return ''
  const date = new Date(Number(micros) / 1000)
  if (!Number.isFinite(date.getTime())) return ''
  return new Date(date.getTime() - date.getTimezoneOffset() * 60_000).toISOString().slice(0, 16)
}

function LoadedSubtotals({
  rows,
  change,
}: {
  rows: WebUsageCallPage['calls']
  change: (patch: Partial<UsageFilters>) => void
}) {
  const sessions = useMemo(() => {
    const groups = new Map<
      string,
      {
        calls: WebUsageCallPage['calls'][number][]
        turns: Map<string | null, WebUsageCallPage['calls'][number][]>
      }
    >()
    for (const row of rows) {
      let session = groups.get(row.session_id)
      if (!session) {
        session = { calls: [], turns: new Map() }
        groups.set(row.session_id, session)
      }
      session.calls.push(row)
      const turn = session.turns.get(row.turn_id) ?? []
      turn.push(row)
      session.turns.set(row.turn_id, turn)
    }
    return [...groups].map(([sessionId, session]) => ({
      sessionId,
      total: costTotalText(totalCost(session.calls, true)),
      turns: [...session.turns].map(([turnId, calls]) => ({
        turnId,
        total: costTotalText(totalCost(calls, true)),
      })),
    }))
  }, [rows])
  return (
    <section aria-label="Loaded cost subtotals">
      <p>These subtotals cover the loaded window. Open a session or turn to see its summary.</p>
      {sessions.map(({ sessionId, total, turns }) => (
        <div key={sessionId}>
          <a href={`/sessions?session=${encodeURIComponent(sessionId)}&workspace=true`}>
            Session {sessionId}
          </a>
          <button type="button" onClick={() => change({ sessionId, turnId: undefined })}>
            Show session usage
          </button>
          <p>{total}</p>
          {turns.map(({ turnId, total }) => (
            <p key={turnId ?? 'session'}>
              {turnId ? (
                <button type="button" onClick={() => change({ sessionId, turnId })}>
                  Turn {turnId}
                </button>
              ) : (
                'Outside a turn'
              )}{' '}
              · {total}
            </p>
          ))}
        </div>
      ))}
    </section>
  )
}
