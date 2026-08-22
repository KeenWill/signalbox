import { useQuery } from '@tanstack/react-query'
import { ArrowRight, Search, X } from 'lucide-react'
import { type FormEvent, useEffect, useRef, useState } from 'react'
import type { WebAttentionSnapshot } from './generated/web-contract.mjs'
import { ProductRequestError, type ProductSessionState, productTransport } from './product'

type SessionSummary = WebAttentionSnapshot['summaries'][number]

const label = (value: string) => value.replaceAll('_', ' ')

const activityTime = (unixMilliseconds: string) => {
  const value = Number(unixMilliseconds)
  if (!Number.isSafeInteger(value)) return unixMilliseconds
  return new Intl.DateTimeFormat('en-US', {
    dateStyle: 'medium',
    timeStyle: 'short',
    timeZone: 'UTC',
  }).format(new Date(value))
}

const SessionTitle = ({ summary }: { summary: SessionSummary }) => (
  <>
    {summary.title_summary ?? 'Untitled session'}
    {summary.title_truncated && <span className="catalog-title-truncated">Truncated</span>}
  </>
)

export function SessionCatalogSurface({
  state,
  onStateChange,
}: {
  state: ProductSessionState
  onStateChange: (state: ProductSessionState) => void
}) {
  const returnFocus = useRef<HTMLButtonElement>(null)
  const closeFocus = useRef<HTMLButtonElement>(null)
  const pageHeading = useRef<HTMLHeadingElement>(null)
  const restorePageFocus = useRef(false)
  const [narrowInspector, setNarrowInspector] = useState(false)
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
          search: state.q,
          sort: state.sort ?? 'activity',
          includeArchived: state.archived ?? false,
          afterSession: state.afterSession,
          afterActivity: state.afterActivity,
        },
        signal,
      ),
    gcTime: 0,
  })
  const selected = sessions.data?.summaries.find((summary) => summary.session_id === state.session)

  useEffect(() => {
    const query = window.matchMedia('(max-width: 760px)')
    const update = () => setNarrowInspector(query.matches)
    update()
    query.addEventListener('change', update)
    return () => query.removeEventListener('change', update)
  }, [])

  useEffect(() => {
    const focusTarget = state.session ? closeFocus : returnFocus
    const frame = requestAnimationFrame(() => focusTarget.current?.focus())
    return () => cancelAnimationFrame(frame)
  }, [state.session])

  useEffect(() => {
    if (!sessions.data || !restorePageFocus.current) return
    restorePageFocus.current = false
    const frame = requestAnimationFrame(() => pageHeading.current?.focus())
    return () => cancelAnimationFrame(frame)
  }, [sessions.data])

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    const form = new FormData(event.currentTarget)
    const q = String(form.get('q') ?? '').trim()
    const sort = form.get('sort') === 'identity' ? 'identity' : undefined
    const archived = form.get('archived') === 'on' ? true : undefined
    onStateChange({ q: q || undefined, sort, archived })
  }
  const openSession = (summary: SessionSummary, button: HTMLButtonElement) => {
    returnFocus.current = button
    onStateChange({ ...state, session: summary.session_id })
  }
  const closeSession = () => onStateChange({ ...state, session: undefined })
  const nextPage = () => {
    const continuation = sessions.data?.continuation
    if (!continuation) return
    restorePageFocus.current = true
    onStateChange({
      q: state.q,
      sort: state.sort,
      archived: state.archived,
      afterSession: continuation.session_id,
      afterActivity:
        continuation.kind === 'last_activity' ? continuation.unix_microseconds : undefined,
    })
  }

  return (
    <div className="surface-body catalog-surface">
      <form
        className="catalog-toolbar"
        onSubmit={submit}
        key={`${state.q}:${state.sort}:${state.archived}`}
      >
        <label className="catalog-search">
          <span>Search titles</span>
          <span>
            <Search aria-hidden="true" />
            <input name="q" defaultValue={state.q} placeholder="Exact title terms" />
          </span>
        </label>
        <label>
          <span>Order</span>
          <select name="sort" defaultValue={state.sort ?? 'activity'}>
            <option value="activity">Recent activity</option>
            <option value="identity">Session identity</option>
          </select>
        </label>
        <label className="catalog-checkbox">
          <input name="archived" type="checkbox" defaultChecked={state.archived} />
          Include archived
        </label>
        <button type="submit">Apply</button>
      </form>

      {sessions.isLoading && <p className="catalog-notice">Reading a bounded session page…</p>}
      {sessions.isError && (
        <section className="surface-empty" role="alert">
          <div>
            <h2>Sessions could not be read</h2>
            <p>
              {sessions.error instanceof ProductRequestError
                ? `${sessions.error.code}: ${sessions.error.message}`
                : 'The response did not match the generated web contract.'}
            </p>
            <button type="button" onClick={() => void sessions.refetch()}>
              Retry
            </button>
          </div>
        </section>
      )}

      {sessions.data && (
        <div className="catalog-workbench">
          <section className="catalog-list" aria-labelledby="catalog-heading">
            <header>
              <div>
                <span className="eyebrow">Bounded session catalog</span>
                <h2 ref={pageHeading} id="catalog-heading" tabIndex={-1}>
                  {sessions.data.total} sessions
                </h2>
              </div>
              <span>{sessions.data.summaries.length} on this page</span>
            </header>
            {sessions.data.summaries.length === 0 ? (
              <p className="catalog-notice">No sessions match the current filters.</p>
            ) : (
              <ol>
                {sessions.data.summaries.map((summary) => (
                  <li key={summary.session_id}>
                    <button
                      type="button"
                      aria-pressed={state.session === summary.session_id}
                      onClick={(event) => openSession(summary, event.currentTarget)}
                    >
                      <span className="catalog-session-copy">
                        <strong>
                          <SessionTitle summary={summary} />
                        </strong>
                        <code>{summary.session_id}</code>
                      </span>
                      <span className={`state-chip state-${summary.state}`}>
                        {label(summary.state)}
                      </span>
                      <span>
                        {summary.active_turn_count} active · {summary.queued_turn_count} queued
                      </span>
                      <time>{activityTime(summary.last_activity.unix_milliseconds)}</time>
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

          {selected && (
            <aside
              className="catalog-inspector"
              role="dialog"
              aria-modal={narrowInspector || undefined}
              aria-labelledby="catalog-inspector-heading"
              onKeyDown={(event) => {
                if (event.key === 'Escape') closeSession()
                if (event.key === 'Tab' && narrowInspector) {
                  event.preventDefault()
                  closeFocus.current?.focus()
                }
              }}
            >
              <header>
                <div>
                  <span className="eyebrow">Selected session</span>
                  <h2 id="catalog-inspector-heading">
                    <SessionTitle summary={selected} />
                  </h2>
                </div>
                <button
                  ref={closeFocus}
                  type="button"
                  aria-label="Close session inspector"
                  onClick={closeSession}
                >
                  <X aria-hidden="true" />
                </button>
              </header>
              <dl>
                <div>
                  <dt>Identity</dt>
                  <dd>
                    <code>{selected.session_id}</code>
                  </dd>
                </div>
                <div>
                  <dt>State</dt>
                  <dd>{label(selected.state)}</dd>
                </div>
                <div>
                  <dt>Activity source</dt>
                  <dd>{label(selected.last_activity.kind)}</dd>
                </div>
                <div>
                  <dt>Active work</dt>
                  <dd>{selected.active_turn_count}</dd>
                </div>
                <div>
                  <dt>Queued work</dt>
                  <dd>{selected.queued_turn_count}</dd>
                </div>
                <div>
                  <dt>Archived</dt>
                  <dd>{selected.archived ? 'Yes' : 'No'}</dd>
                </div>
              </dl>
              {selected.goal_block && (
                <section>
                  <span className="eyebrow">Blocked goal</span>
                  <p>{selected.goal_block.need_summary}</p>
                </section>
              )}
              <p className="catalog-address-note">
                Timeline opening remains on its isolated Session Workspace integration branch.
              </p>
            </aside>
          )}
        </div>
      )}
    </div>
  )
}
