import { useQuery } from '@tanstack/react-query'
import { useNavigate } from '@tanstack/react-router'
import { ArrowRight, Search } from 'lucide-react'
import { type FormEvent, useEffect, useMemo, useRef, useState } from 'react'
import type { WebSessionCatalogSnapshot } from './generated/web-contract.mjs'
import { enumLabel } from './labels'
import {
  ProductRequestError,
  type ProductSessionState,
  ProductTransportError,
  productTransport,
  readSessionRates,
  readSessionTranscript,
  type SessionTranscriptLimits,
} from './product'
import { HttpSessionTimelineSource } from './session-timeline/model'
import { actions, useAppDispatch, useAppSelector } from './state'
import './catalog.css'

type SessionSummary = WebSessionCatalogSnapshot['summaries'][number]

const activityTime = (unixMicroseconds: string) => {
  const value = Number(BigInt(unixMicroseconds) / BigInt(1000))
  if (!Number.isSafeInteger(value)) return unixMicroseconds
  return new Intl.DateTimeFormat('en-US', {
    dateStyle: 'medium',
    timeStyle: 'short',
    timeZone: 'UTC',
  }).format(new Date(value))
}

// WebSessionCatalogSummary limits titles to 128 Unicode scalars.
const MAX_SESSION_SUMMARY_SCALARS = 128

async function readAutomaticTitle(
  sessionId: string,
  source: HttpSessionTimelineSource,
  limits: SessionTranscriptLimits,
  signal: AbortSignal,
) {
  const descriptor = await source.readDescriptor(sessionId, signal)
  const pullRequest = descriptor.repository_watch?.pull_request
  const prefix = pullRequest ? `PR #${pullRequest}` : ''
  const page = await readSessionTranscript(
    sessionId,
    descriptor.first_address.event_sequence,
    descriptor.latest_address.event_sequence,
    null,
    limits,
    signal,
  )
  const message = page.items.find((item) => item.body.type === 'user_input')?.body
  const text = message?.type === 'user_input' ? message.text.text.trim().split('\n', 1)[0] : ''
  const scalars = Array.from([prefix, text].filter(Boolean).join(': '))
  return {
    text: scalars.slice(0, MAX_SESSION_SUMMARY_SCALARS).join(''),
    truncated: scalars.length > MAX_SESSION_SUMMARY_SCALARS,
  }
}

const SessionTitle = ({
  summary,
  source,
  limits,
}: {
  summary: SessionSummary
  source?: HttpSessionTimelineSource
  limits?: SessionTranscriptLimits
}) => {
  const automatic = useQuery({
    queryKey: ['production', 'automatic-session-title', summary.session_id],
    queryFn: ({ signal }) =>
      source && limits ? readAutomaticTitle(summary.session_id, source, limits, signal) : null,
    enabled: !summary.title_summary?.trim() && source !== undefined && limits !== undefined,
    gcTime: 0,
  })
  const savedTitle = summary.title_summary?.trim()
  const truncated = savedTitle ? summary.title_truncated : automatic.data?.truncated
  return (
    <>
      {savedTitle || automatic.data?.text || `Session ${summary.session_id}`}
      {truncated && <span className="catalog-title-truncated">Truncated</span>}
    </>
  )
}

export function SessionCatalogSurface({
  returnSessionId,
  onReturnFocusConsumed,
  lifecycleFilter,
  pageOrder,
  onLifecycleFilterChange,
  onPageOrderChange,
  state,
  onStateChange,
  onTimelineIds,
}: {
  returnSessionId?: string
  onReturnFocusConsumed: () => void
  lifecycleFilter: string
  pageOrder: string
  onLifecycleFilterChange: (value: string) => void
  onPageOrderChange: (value: string) => void
  state: ProductSessionState
  onTimelineIds: (ids: readonly string[]) => void
  onStateChange: (state: ProductSessionState, mode?: 'push' | 'close' | 'replace') => void
}) {
  const navigate = useNavigate()
  const bootstrap = useQuery({
    queryKey: ['production', 'bootstrap'],
    queryFn: ({ signal }) => productTransport.readBootstrap(signal),
    staleTime: Number.POSITIVE_INFINITY,
  })
  const searchAvailable = bootstrap.data?.capabilities.bounded_lexical_search === true

  const dispatch = useAppDispatch()
  const keyboardSelection = useAppSelector((root) => root.app.selectedTimeline)
  const sessionButtons = useRef(new Map<string, HTMLButtonElement>())
  const pageHeading = useRef<HTMLHeadingElement>(null)
  const errorHeading = useRef<HTMLHeadingElement>(null)
  const restorePageFocus = useRef(false)
  const pendingReturnFocus = useRef(returnSessionId)
  const [searchError, setSearchError] = useState<string | null>(null)
  const overlay = useAppSelector((root) => root.app.overlay)
  const sessions = useQuery({
    queryKey: [
      'production',
      'sessions',
      state.q ?? null,
      state.sort ?? 'activity',
      state.archived ?? false,
      state.afterSession ?? null,
      state.afterActivity ?? null,
    ],
    queryFn: ({ signal }) =>
      productTransport.readSessions(
        {
          sort: state.sort ?? 'activity',
          includeArchived: state.archived ?? false,
          afterSession: state.afterSession,
          afterActivity: state.afterActivity,
        },
        signal,
      ),
    gcTime: 0,
  })
  const titleSource = useQuery({
    queryKey: ['production', 'catalog-title-source'],
    queryFn: ({ signal }) => HttpSessionTimelineSource.connect(window.fetch.bind(window), signal),
    enabled: sessions.data?.summaries.some((row) => !row.title_summary?.trim()) === true,
    staleTime: Number.POSITIVE_INFINITY,
    gcTime: 0,
  })
  const sessionIds = useMemo(
    () => sessions.data?.summaries.map((row) => row.session_id) ?? [],
    [sessions.data],
  )
  const rates = useQuery({
    queryKey: ['production', 'session-rates', sessionIds],
    queryFn: ({ signal }) => readSessionRates(sessionIds, signal),
    enabled: sessions.data !== undefined,
    gcTime: 0,
  })
  const rateById = useMemo(
    () => new Map(rates.data?.sessions.map((row) => [row.session_id, row])),
    [rates.data],
  )
  const listed = useMemo(() => {
    const rows = (sessions.data?.summaries ?? []).filter(
      (row) =>
        rates.data === undefined ||
        lifecycleFilter === 'all' ||
        rateById.get(row.session_id)?.lifecycle_state === lifecycleFilter,
    )
    if (pageOrder === 'failure' && rates.data !== undefined)
      rows.sort((a, b) => {
        const left = BigInt(rateById.get(a.session_id)?.last_failure_sequence ?? '0')
        const right = BigInt(rateById.get(b.session_id)?.last_failure_sequence ?? '0')
        return left === right ? a.session_id.localeCompare(b.session_id) : left > right ? -1 : 1
      })
    return rows
  }, [sessions.data, rates.data, lifecycleFilter, pageOrder, rateById])
  useEffect(() => {
    onTimelineIds(listed.map((row) => row.session_id))
    return () => onTimelineIds([])
  }, [listed, onTimelineIds])
  useEffect(() => {
    if (keyboardSelection && overlay === null)
      sessionButtons.current.get(keyboardSelection)?.focus()
  }, [keyboardSelection, overlay])
  useEffect(() => {
    if (!sessions.data || overlay !== null || !pendingReturnFocus.current) return
    const target = sessionButtons.current.get(pendingReturnFocus.current)
    pendingReturnFocus.current = undefined
    target?.focus()
    onReturnFocusConsumed()
  }, [sessions.data, overlay, onReturnFocusConsumed])
  useEffect(() => {
    if (!sessions.data || !restorePageFocus.current) return
    restorePageFocus.current = false
    const frame = requestAnimationFrame(() => pageHeading.current?.focus())
    return () => cancelAnimationFrame(frame)
  }, [sessions.data])

  useEffect(() => {
    if (!sessions.isError || !restorePageFocus.current) return
    restorePageFocus.current = false
    const frame = requestAnimationFrame(() => errorHeading.current?.focus())
    return () => cancelAnimationFrame(frame)
  }, [sessions.isError])

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    const form = new FormData(event.currentTarget)
    const q = String(form.get('q') ?? '').trim()
    const queryLimit = bootstrap.data?.limits.max_search_query_bytes ?? 0
    if (
      q &&
      (!searchAvailable || new TextEncoder().encode(q).length > queryLimit || q.includes('\0'))
    ) {
      setSearchError('Check your search. Try a shorter phrase.')
      return
    }
    setSearchError(null)
    if (q.trim()) {
      void navigate({ to: '/$surface', params: { surface: 'search' }, search: { q: q.trim() } })
      return
    }
    const sort = form.get('sort') === 'identity' ? 'identity' : undefined
    const archived = form.get('archived') === 'on' ? true : undefined
    if (q === (state.q ?? '') && sort === state.sort && archived === state.archived) return
    restorePageFocus.current = true
    onStateChange({ q: q || undefined, sort, archived })
  }
  const nextPage = () => {
    const continuation = sessions.data?.continuation
    if (!continuation) return
    restorePageFocus.current = true
    onStateChange(
      {
        q: state.q,
        sort: state.sort,
        archived: state.archived,
        afterSession: continuation.session_id,
        afterActivity:
          continuation.kind === 'last_activity' ? continuation.unix_microseconds : undefined,
      },
      'replace',
    )
  }

  return (
    <div className="surface-body catalog-surface">
      <form
        className="catalog-toolbar"
        onSubmit={submit}
        key={JSON.stringify([state.q ?? null, state.sort ?? null, state.archived ?? null])}
      >
        <label className="catalog-search">
          <span>Search conversations</span>
          <span>
            <Search aria-hidden="true" />
            <input
              name="q"
              disabled={!searchAvailable}
              defaultValue={state.q}
              placeholder="Messages, tool arguments and results"
              onKeyDown={(event) => {
                if (event.key !== 'Escape') return
                event.currentTarget.closest('main')?.focus()
              }}
            />
          </span>
        </label>
        <label>
          <span>Order</span>
          <select name="sort" defaultValue={state.sort ?? 'activity'}>
            <option value="activity">Recent activity</option>
            <option value="identity">Session ID</option>
          </select>
        </label>
        <label className="catalog-checkbox">
          <input name="archived" type="checkbox" defaultChecked={state.archived} />
          Include archived
        </label>
        <button type="submit">Apply</button>
      </form>
      {searchError && (
        <p className="catalog-notice" role="alert">
          {searchError}
        </p>
      )}

      {sessions.isLoading && <p className="catalog-notice">Loading sessions…</p>}
      {sessions.isError && (
        <section className="surface-empty" role="alert">
          <div>
            <h2 ref={errorHeading} tabIndex={-1}>
              Sessions failed to load
            </h2>
            <p>
              {sessions.error instanceof ProductRequestError
                ? `${sessions.error.response.error.code}: ${sessions.error.message}`
                : sessions.error instanceof ProductTransportError
                  ? sessions.error.message
                  : 'Unexpected daemon response.'}
            </p>
            <button
              type="button"
              onClick={() => {
                restorePageFocus.current = true
                void sessions.refetch()
              }}
            >
              Retry
            </button>
          </div>
        </section>
      )}

      {sessions.data && (
        <section className="catalog-rates" aria-label="Session outcomes">
          <div className="catalog-rate-controls">
            <label>
              State{' '}
              <select
                value={lifecycleFilter}
                onChange={(event) => onLifecycleFilterChange(event.target.value)}
              >
                <option value="all">All states</option>
                {[
                  'created',
                  'dispatched',
                  'active',
                  'waiting',
                  'recovering',
                  'blocked',
                  'parked',
                  'terminal',
                ].map((state) => (
                  <option key={state} value={state}>
                    {enumLabel(state)}
                  </option>
                ))}
              </select>
            </label>
            <label>
              Page order{' '}
              <select value={pageOrder} onChange={(event) => onPageOrderChange(event.target.value)}>
                <option value="activity">Default</option>
                <option value="failure">Last failure</option>
              </select>
            </label>
          </div>
          {rates.isPending ? (
            <p>Loading outcomes…</p>
          ) : rates.isError ? (
            <p role="alert">
              Session outcomes unavailable.{' '}
              <button type="button" onClick={() => void rates.refetch()}>
                Retry outcomes
              </button>
            </p>
          ) : null}
        </section>
      )}
      {sessions.data && (
        <div className="catalog-workbench">
          <section className="catalog-list" aria-labelledby="catalog-heading">
            <header>
              <div>
                <h2 ref={pageHeading} id="catalog-heading" tabIndex={-1}>
                  {sessions.data.total} {sessions.data.total === '1' ? 'session' : 'sessions'}
                </h2>
              </div>
              <div className="catalog-header-actions">
                <span>{sessions.data.summaries.length} on this page</span>
              </div>
            </header>
            {listed.length === 0 ? (
              <p className="catalog-notice">No matching sessions</p>
            ) : (
              <ol>
                {listed.map((summary) => (
                  <li key={summary.session_id}>
                    <button
                      ref={(button) => {
                        if (button) sessionButtons.current.set(summary.session_id, button)
                        else sessionButtons.current.delete(summary.session_id)
                      }}
                      type="button"
                      onFocus={() => dispatch(actions.timelineSelected(summary.session_id))}
                      onClick={() =>
                        onStateChange({ ...state, session: summary.session_id, workspace: true })
                      }
                    >
                      <span className="catalog-session-copy">
                        <strong>
                          <SessionTitle
                            summary={summary}
                            source={titleSource.data}
                            limits={bootstrap.data?.limits}
                          />
                        </strong>
                        <code>{summary.session_id}</code>
                        {summary.action && <small>{enumLabel(summary.action)}</small>}
                        {summary.goal_block && (
                          <small>
                            Blocked: {enumLabel(summary.goal_block.reason)} —{' '}
                            {summary.goal_block.need_summary}
                          </small>
                        )}
                      </span>
                      <span
                        className={`state-chip state-${rateById.get(summary.session_id)?.lifecycle_state ?? 'unavailable'}`}
                      >
                        {enumLabel(
                          rateById.get(summary.session_id)?.lifecycle_state ?? 'unavailable',
                        )}
                      </span>
                      <span>
                        {rateById.has(summary.session_id) ? (
                          <>
                            {rateById.get(summary.session_id)?.turn_count} turns ·{' '}
                            {rateById.get(summary.session_id)?.failed_turn_count} failed
                            <small>
                              {enumLabel(
                                rateById.get(summary.session_id)?.last_provider_cause ??
                                  (rateById.get(summary.session_id)?.last_failure_sequence
                                    ? 'No cause recorded'
                                    : 'No failures'),
                              )}
                            </small>
                            {rateById.get(summary.session_id)?.goal_disposition && (
                              <small>
                                Goal:{' '}
                                {enumLabel(
                                  rateById.get(summary.session_id)?.goal_disposition ?? '',
                                )}
                              </small>
                            )}
                          </>
                        ) : (
                          `${summary.active_turn_count} active · ${summary.queued_turn_count} queued`
                        )}
                      </span>
                      <time title="Last activity">
                        {activityTime(summary.last_activity.unix_microseconds)}
                      </time>
                      <ArrowRight aria-hidden="true" />
                    </button>
                  </li>
                ))}
              </ol>
            )}
            {sessions.data.continuation && (
              <button className="catalog-next" type="button" onClick={nextPage}>
                Next page <ArrowRight aria-hidden="true" />
              </button>
            )}
          </section>
        </div>
      )}
    </div>
  )
}
