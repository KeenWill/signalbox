import { useQuery } from '@tanstack/react-query'
import { createColumnHelper, tableFeatures, useTable } from '@tanstack/react-table'
import { useVirtualizer } from '@tanstack/react-virtual'
import { ArrowDown, ArrowRight, ExternalLink, RefreshCw } from 'lucide-react'
import { type RefObject, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import type {
  WebRepoWatchActivityPage,
  WebRepoWatchPullRequestPage,
  WebRepoWatchPullRequestSessionPage,
  WebRepoWatchRepositoryStatusPage,
  WebRepoWatchWorkPage,
} from './generated/web-contract.mjs'
import {
  ProductRequestError,
  productTransport,
  type RepoWatchActivityWindow,
  type RepoWatchEventCursor,
  type RepoWatchHeldCursor,
  type RepoWatchObligationCursor,
  type RepoWatchSessionCursor,
} from './product'
import { actions, useAppDispatch } from './state'

type RepositoryStatus = WebRepoWatchRepositoryStatusPage['repositories'][number]
type PullRequest = WebRepoWatchPullRequestPage['pull_requests'][number]
type PullRequestSession = WebRepoWatchPullRequestSessionPage['sessions'][number]
type SingletonScope = WebRepoWatchWorkPage['held_slots'][number]['scope']

interface ActivityRow {
  id: string
  time: string
  source: 'repository event' | 'webhook delivery'
  kind: string
  subject: string
  cursorOrOutcome: string
}

interface RetainedActivityPage {
  key: string
  page: WebRepoWatchActivityPage
}

export const retainActivityPage = (
  pages: RetainedActivityPage[],
  key: string,
  page: WebRepoWatchActivityPage,
): RetainedActivityPage[] => {
  const existing = pages.findIndex((candidate) => candidate.key === key)
  const next = [...pages]
  if (existing === -1) next.push({ key, page })
  else next[existing] = { key, page }
  return next.slice(-8)
}

// Tunable effective ceiling: virtual rows retain a small viewport overscan.
const ACTIVITY_OVERSCAN_ROWS = 10

const historyFeatures = tableFeatures({})
const historyColumn = createColumnHelper<typeof historyFeatures, ActivityRow>()
const historyColumns = historyColumn.columns([
  historyColumn.accessor('time', { header: 'Observed' }),
  historyColumn.accessor('source', { header: 'Source' }),
  historyColumn.accessor('kind', { header: 'Kind' }),
  historyColumn.accessor('subject', { header: 'Subject' }),
  historyColumn.accessor('cursorOrOutcome', { header: 'Cursor / outcome' }),
])

const words = (value: string) => value.replaceAll('_', ' ')

export const singletonScopeLabel = (scope: SingletonScope) => {
  switch (scope.kind) {
    case 'pull_request':
      return `PR ${scope.repository}#${scope.number}`
    case 'stack':
      return `Stack ${scope.repository}#${scope.root_pull_request}`
    case 'rule':
      return 'Rule-wide'
    case 'repository':
      return `Repository ${scope.repository}`
  }
}

const time = (unixMilliseconds: string | null | undefined) => {
  if (!unixMilliseconds) return '—'
  const value = Number(unixMilliseconds)
  if (!Number.isSafeInteger(value)) return unixMilliseconds
  return new Intl.DateTimeFormat('en-US', {
    dateStyle: 'medium',
    timeStyle: 'medium',
    timeZone: 'UTC',
  }).format(new Date(value))
}

const durationLabel = (seconds: number) => {
  if (seconds % 3_600 === 0) return `${seconds / 3_600}h`
  if (seconds % 60 === 0) return `${seconds / 60}m`
  return `${seconds}s`
}

const errorMessage = (error: unknown) =>
  error instanceof ProductRequestError
    ? `${error.code}: ${error.message}`
    : 'The daemon response did not match the generated browser contract.'

function HistoryTable({ rows }: { rows: ActivityRow[] }) {
  'use no memo'
  const dispatch = useAppDispatch()
  const table = useTable({ data: rows, columns: historyColumns, features: historyFeatures })
  const tableRows = table.getRowModel().rows
  const parentRef = useRef<HTMLDivElement>(null)
  const virtualizer = useVirtualizer({
    count: tableRows.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 38,
    overscan: ACTIVITY_OVERSCAN_ROWS,
    getItemKey: (index) => tableRows[index]?.original.id ?? index,
  })
  const virtualRows = virtualizer.getVirtualItems()
  const rangeStart = virtualRows[0]?.index ?? 0
  const rangeEnd = virtualRows.at(-1)?.index ?? 0

  useEffect(() => {
    dispatch(actions.tableRangeSet({ start: rangeStart, end: rangeEnd }))
  }, [dispatch, rangeEnd, rangeStart])

  return (
    // biome-ignore lint/a11y/useSemanticElements: Virtualization requires a scrollable ARIA table.
    <div className="activity-history-table" role="table" aria-label="Repository activity history">
      {/* biome-ignore lint/a11y: Virtualized column headers are owned by the ARIA table. */}
      <div className="activity-history-header" role="row" aria-rowindex={1}>
        {table.getHeaderGroups()[0]?.headers.map((header) => (
          // biome-ignore lint/a11y: Virtualized ARIA column headers are not interactive.
          <div role="columnheader" key={header.id}>
            <table.FlexRender header={header} />
          </div>
        ))}
      </div>
      {/* biome-ignore lint/a11y/useSemanticElements: Virtualization requires a div-backed ARIA rowgroup. */}
      <div
        ref={parentRef}
        className="activity-history-scroll"
        role="rowgroup"
        // biome-ignore lint/a11y/noNoninteractiveTabindex: Native keyboard scrolling exposes virtualized rows beyond the mounted range.
        tabIndex={0}
        aria-label="Scrollable repository activity rows"
        data-mounted-rows={virtualRows.length}
        data-loaded-rows={rows.length}
      >
        <div className="activity-history-stage" style={{ height: virtualizer.getTotalSize() }}>
          {virtualRows.map((virtualRow) => {
            const row = tableRows[virtualRow.index]
            if (!row) return null
            return (
              // biome-ignore lint/a11y: Focus lives on the row's activity link.
              <div
                id={`activity-${row.original.id}`}
                className="activity-history-row"
                role="row"
                aria-rowindex={virtualRow.index + 2}
                key={row.original.id}
                style={{
                  height: virtualRow.size,
                  transform: `translateY(${virtualRow.start}px)`,
                }}
              >
                {row.getAllCells().map((cell, index) => (
                  // biome-ignore lint/a11y/useSemanticElements: Virtual rows cannot use native cells.
                  <div role="cell" key={cell.id}>
                    {index === 0 ? (
                      <a href={`#activity-${row.original.id}`}>
                        <table.FlexRender cell={cell} />
                      </a>
                    ) : (
                      <table.FlexRender cell={cell} />
                    )}
                  </div>
                ))}
              </div>
            )
          })}
        </div>
      </div>
    </div>
  )
}

function RepositoryHealth({ status }: { status: RepositoryStatus }) {
  return (
    <section className="activity-health" aria-labelledby="ingestion-health-heading">
      <header>
        <div>
          <span className="eyebrow">Durable ingestion</span>
          <h2 id="ingestion-health-heading">{status.repository}</h2>
        </div>
        <span>cursor {status.cursor_generation ?? 'not observed'}</span>
      </header>
      <dl>
        <div>
          <dt>Latest repository observation</dt>
          <dd>{time(status.observed_at_unix_milliseconds)}</dd>
        </div>
        <div>
          <dt>Latest webhook</dt>
          <dd>{time(status.latest_webhook?.received_at_unix_milliseconds)}</dd>
        </div>
        <div>
          <dt>
            Delivery volume · {durationLabel(status.previous_five_minutes.seconds)} /{' '}
            {durationLabel(status.previous_hour.seconds)}
          </dt>
          <dd>
            {status.previous_five_minutes.received} / {status.previous_hour.received}
          </dd>
        </div>
        <div>
          <dt>Projection latency · latest / 1h max</dt>
          <dd>
            {status.latest_projection_latency_milliseconds ?? '—'} /{' '}
            {status.maximum_projection_latency_milliseconds_previous_hour ?? '—'} ms
          </dd>
        </div>
        <div>
          <dt>Held / queued</dt>
          <dd>
            {status.held_slot_count} / {status.queued_obligation_count}
          </dd>
        </div>
      </dl>
      {/* biome-ignore lint/a11y/useSemanticElements: These read-only facts are not form controls. */}
      <div className="activity-facts" role="group" aria-label="Distinct event and action facts">
        <span>
          Observed: {status.last_observed_event ? words(status.last_observed_event.kind) : '—'}
        </span>
        <span>
          Actionable:{' '}
          {status.last_actionable_event ? words(status.last_actionable_event.kind) : '—'}
        </span>
        <span>
          Dispatch:{' '}
          {status.last_dispatch_attempt
            ? time(status.last_dispatch_attempt.attempted_at_unix_milliseconds)
            : '—'}
        </span>
        <span>
          Settled:{' '}
          {status.last_automation_settlement
            ? time(status.last_automation_settlement.settled_at_unix_milliseconds)
            : '—'}
        </span>
      </div>
      <p className="activity-kind-line">
        {status.event_kind_counts_previous_hour
          .map((item) => `${words(item.kind)} ${item.count}`)
          .join(' · ') || 'No event kinds in the previous hour'}
      </p>
    </section>
  )
}

function PullRequestTable({
  page,
  repository,
  selected,
  onSelect,
  headingRef,
}: {
  page: WebRepoWatchPullRequestPage
  repository: string
  selected: string | null
  onSelect: (pullRequest: string) => void
  headingRef: RefObject<HTMLHeadingElement | null>
}) {
  return (
    <section className="activity-table-panel" aria-labelledby="pull-requests-heading">
      <header>
        <div>
          <span className="eyebrow">Current provider and automation facts</span>
          <h2 id="pull-requests-heading" ref={headingRef} tabIndex={-1}>
            Pull requests
          </h2>
        </div>
        <span>{page.pull_requests.length} loaded</span>
      </header>
      <div className="activity-table-scroll">
        <table>
          <thead>
            <tr>
              <th scope="col">PR</th>
              <th scope="col">Head → base</th>
              <th scope="col">Provider</th>
              <th scope="col">Review evidence</th>
              <th scope="col">Automation</th>
              <th scope="col">Stack</th>
              <th scope="col">Sessions</th>
            </tr>
          </thead>
          <tbody>
            {page.pull_requests.map((pullRequest) => (
              <tr key={pullRequest.number} data-selected={selected === pullRequest.number}>
                <td>
                  <button
                    type="button"
                    aria-controls="pr-sessions-panel"
                    aria-expanded={selected === pullRequest.number}
                    aria-pressed={selected === pullRequest.number}
                    onClick={() => onSelect(pullRequest.number)}
                  >
                    #{pullRequest.number} {pullRequest.title}
                  </button>
                  <a
                    href={`https://github.com/${repository}/pull/${pullRequest.number}`}
                    target="_blank"
                    rel="noreferrer"
                    aria-label={`Open pull request ${pullRequest.number} on GitHub`}
                  >
                    <ExternalLink aria-hidden="true" />
                  </a>
                </td>
                <td>
                  <code>{pullRequest.head.slice(0, 8)}</code> {pullRequest.head_repository}:
                  {pullRequest.head_branch} → {pullRequest.base_branch}
                </td>
                <td>
                  {words(pullRequest.lifecycle)} · {words(pullRequest.mergeable)} ·{' '}
                  {words(pullRequest.checks)}
                </td>
                <td>
                  <a
                    href={`https://github.com/${repository}/pull/${pullRequest.number}#pullrequestreview`}
                    target="_blank"
                    rel="noreferrer"
                  >
                    {words(pullRequest.review_decision)} · {pullRequest.unresolved_thread_count}{' '}
                    unresolved · {pullRequest.stale_review_count} stale
                  </a>
                </td>
                <td>
                  {words(pullRequest.automation.kind)} · {pullRequest.held_slot_count} held ·{' '}
                  {pullRequest.queued_obligation_count} queued
                </td>
                <td>
                  parent {pullRequest.open_parent ?? '—'} · children {pullRequest.open_child_count}
                </td>
                <td>{pullRequest.commissioned_session_count}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  )
}

function WorkTables({ page }: { page: WebRepoWatchWorkPage }) {
  return (
    <div className="activity-work-grid">
      <section className="activity-table-panel" aria-labelledby="held-work-heading">
        <header>
          <div>
            <span className="eyebrow">Occupied dispatch slots</span>
            <h2 id="held-work-heading">Held work</h2>
          </div>
          <span>{page.held_slots.length}</span>
        </header>
        <table>
          <thead>
            <tr>
              <th scope="col">PR / rule</th>
              <th scope="col">Held</th>
              <th scope="col">Typed blockers</th>
            </tr>
          </thead>
          <tbody>
            {page.held_slots.map((slot) => (
              <tr key={slot.dispatch_id}>
                <td>
                  {singletonScopeLabel(slot.scope)} · {slot.rule}
                </td>
                <td>{time(slot.held_since_unix_milliseconds)}</td>
                <td>{slot.blockers.map(words).join(', ') || 'No blocker'}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </section>
      <section className="activity-table-panel" aria-labelledby="queued-work-heading">
        <header>
          <div>
            <span className="eyebrow">Unsettled obligations</span>
            <h2 id="queued-work-heading">Queued work</h2>
          </div>
          <span>{page.queued_obligations.length}</span>
        </header>
        <table>
          <thead>
            <tr>
              <th scope="col">PR / rule</th>
              <th scope="col">Wait</th>
              <th scope="col">Events</th>
              <th scope="col">Readiness</th>
            </tr>
          </thead>
          <tbody>
            {page.queued_obligations.map((obligation) => (
              <tr key={obligation.id}>
                <td>
                  {singletonScopeLabel(obligation.scope)} · {obligation.rule}
                </td>
                <td>{time(obligation.owed_since_unix_milliseconds)}</td>
                <td>{obligation.matched_event_count}</td>
                <td>{words(obligation.readiness.kind)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </section>
    </div>
  )
}

function SessionPanel({
  pullRequest,
  query,
}: {
  pullRequest: PullRequest
  query: ReturnType<typeof useQuery<WebRepoWatchPullRequestSessionPage>>
}) {
  return (
    <section
      id="pr-sessions-panel"
      className="activity-session-panel"
      aria-labelledby="pr-sessions-heading"
    >
      <header>
        <div>
          <span className="eyebrow">PR #{pullRequest.number}</span>
          <h2 id="pr-sessions-heading">Commissioned sessions</h2>
        </div>
        <span>{pullRequest.commissioned_session_count} total</span>
      </header>
      {query.isLoading && <p>Reading correlated sessions…</p>}
      {query.isError && <p role="alert">{errorMessage(query.error)}</p>}
      <ol>
        {query.data?.sessions.map((session: PullRequestSession) => (
          <li key={session.attention.session_id}>
            <div>
              <strong>{words(session.attention.state)}</strong>
              <code>{session.attention.session_id}</code>
            </div>
            <span>
              {words(session.purpose.kind)} · {session.purpose.template}
            </span>
            <span>
              {session.attention.action ? words(session.attention.action) : 'Observe only'}
            </span>
            <a href="/attention">Attention fleet</a>
          </li>
        ))}
      </ol>
    </section>
  )
}

export function ActivitySurface() {
  const [repositoryAfter, setRepositoryAfter] = useState<string | null>(null)
  const [repository, setRepository] = useState<string | null>(null)
  const [pullRequestAfter, setPullRequestAfter] = useState<string | null>(null)
  const [heldAfter, setHeldAfter] = useState<RepoWatchHeldCursor | undefined>()
  const [obligationAfter, setObligationAfter] = useState<RepoWatchObligationCursor | undefined>()
  const [selectedPullRequest, setSelectedPullRequest] = useState<string | null>(null)
  const [sessionBefore, setSessionBefore] = useState<RepoWatchSessionCursor | undefined>()
  const [eventBefore, setEventBefore] = useState<RepoWatchEventCursor | undefined>()
  const [webhookBefore, setWebhookBefore] = useState<string | undefined>()
  const [includeEvents, setIncludeEvents] = useState(true)
  const [includeWebhooks, setIncludeWebhooks] = useState(true)
  const [activityPaging, setActivityPaging] = useState(false)
  const [activityPages, setActivityPages] = useState<RetainedActivityPage[]>([])
  const [filter, setFilter] = useState('')
  const [sort, setSort] = useState<'newest' | 'oldest'>('newest')
  const pullRequestHeadingFocus = useRef<HTMLHeadingElement>(null)
  const pullRequestFocusPending = useRef<string | null | undefined>(undefined)
  const repositorySelectFocus = useRef<HTMLSelectElement>(null)
  const repositoryFocusPending = useRef<string | null | undefined>(undefined)

  const repositories = useQuery({
    queryKey: ['production', 'repository-watch', 'repositories', repositoryAfter],
    queryFn: ({ signal }) =>
      productTransport.readRepoWatchRepositories(repositoryAfter ?? undefined, signal),
  })
  useEffect(() => {
    const first = repositories.data?.repositories[0]?.repository
    const selectedIsLoaded = repositories.data?.repositories.some(
      (item) => item.repository === repository,
    )
    if (first && !selectedIsLoaded) {
      setRepository(first)
      setPullRequestAfter(null)
      setSelectedPullRequest(null)
      setHeldAfter(undefined)
      setObligationAfter(undefined)
      setSessionBefore(undefined)
      setEventBefore(undefined)
      setWebhookBefore(undefined)
      setIncludeEvents(true)
      setIncludeWebhooks(true)
      setActivityPaging(false)
      setActivityPages([])
    }
  }, [repositories.data, repository])

  useLayoutEffect(() => {
    if (repositoryFocusPending.current !== repositoryAfter || !repositories.data) return
    repositoryFocusPending.current = undefined
    repositorySelectFocus.current?.focus()
  }, [repositories.data, repositoryAfter])

  const pullRequests = useQuery({
    queryKey: ['production', 'repository-watch', 'pull-requests', repository, pullRequestAfter],
    queryFn: ({ signal }) =>
      productTransport.readRepoWatchPullRequests(
        repository ?? '',
        pullRequestAfter ?? undefined,
        signal,
      ),
    enabled: repository !== null,
  })
  const work = useQuery({
    queryKey: ['production', 'repository-watch', 'work', repository, heldAfter, obligationAfter],
    queryFn: ({ signal }) =>
      productTransport.readRepoWatchWork(repository ?? '', heldAfter, obligationAfter, signal),
    enabled: repository !== null,
  })
  const sessions = useQuery({
    queryKey: [
      'production',
      'repository-watch',
      'sessions',
      repository,
      selectedPullRequest,
      sessionBefore,
    ],
    queryFn: ({ signal }) =>
      productTransport.readRepoWatchPullRequestSessions(
        repository ?? '',
        selectedPullRequest ?? '',
        sessionBefore,
        signal,
      ),
    enabled: repository !== null && selectedPullRequest !== null,
  })
  const activity = useQuery({
    queryKey: [
      'production',
      'repository-watch',
      'activity',
      repository,
      eventBefore,
      webhookBefore,
      includeEvents,
      includeWebhooks,
    ],
    queryFn: ({ signal }) =>
      productTransport.readRepoWatchActivity(
        repository ?? '',
        {
          eventBefore,
          webhookBeforeReceiptSequence: webhookBefore,
          includeEvents,
          includeWebhooks,
        },
        signal,
      ),
    enabled: repository !== null,
    gcTime: 0,
  })

  useEffect(() => {
    if (!activity.data) return
    const key = JSON.stringify([eventBefore, webhookBefore, includeEvents, includeWebhooks])
    setActivityPages((pages) =>
      eventBefore || webhookBefore
        ? retainActivityPage(pages, key, activity.data)
        : [{ key, page: activity.data }],
    )
  }, [activity.data, eventBefore, includeEvents, includeWebhooks, webhookBefore])

  const status = repositories.data?.repositories.find((item) => item.repository === repository)
  const selected = pullRequests.data?.pull_requests.find(
    (item) => item.number === selectedPullRequest,
  )
  useLayoutEffect(() => {
    if (pullRequestFocusPending.current !== pullRequestAfter || !pullRequests.data) return
    pullRequestFocusPending.current = undefined
    pullRequestHeadingFocus.current?.focus()
  }, [pullRequestAfter, pullRequests.data])
  const rows = useMemo(() => {
    const unique = new Map<string, ActivityRow>()
    for (const retained of activityPages) {
      const page = retained.page
      for (const event of page.events) {
        unique.set(`event-${event.id}`, {
          id: `event-${event.id}`,
          time: event.observed_at_unix_milliseconds,
          source: 'repository event',
          kind: words(event.kind),
          subject: event.pull_request ? `PR #${event.pull_request}` : (repository ?? 'repository'),
          cursorOrOutcome: `${event.cursor_generation}.${event.event_ordinal}`,
        })
      }
      for (const webhook of page.webhooks) {
        unique.set(`webhook-${webhook.receipt_sequence}`, {
          id: `webhook-${webhook.receipt_sequence}`,
          time: webhook.received_at_unix_milliseconds,
          source: 'webhook delivery',
          kind: webhook.action_name
            ? `${webhook.event_name}.${webhook.action_name}`
            : webhook.event_name,
          subject: `delivery ${webhook.receipt_sequence}`,
          cursorOrOutcome: webhook.disposition ? words(webhook.disposition) : 'pending',
        })
      }
    }
    const normalized = filter.trim().toLowerCase()
    return [...unique.values()]
      .filter(
        (row) =>
          !normalized ||
          Object.values(row).some((value) => value.toLowerCase().includes(normalized)),
      )
      .sort((left, right) =>
        sort === 'newest'
          ? Number(right.time) - Number(left.time)
          : Number(left.time) - Number(right.time),
      )
  }, [activityPages, filter, repository, sort])

  const chooseRepository = (next: string) => {
    setRepository(next)
    setPullRequestAfter(null)
    setSelectedPullRequest(null)
    setHeldAfter(undefined)
    setObligationAfter(undefined)
    setSessionBefore(undefined)
    setEventBefore(undefined)
    setWebhookBefore(undefined)
    setIncludeEvents(true)
    setIncludeWebhooks(true)
    setActivityPaging(false)
    setActivityPages([])
  }
  const changePullRequestPage = (after: string | null) => {
    setSelectedPullRequest(null)
    setSessionBefore(undefined)
    pullRequestFocusPending.current = after
    setPullRequestAfter(after)
  }
  const changeRepositoryPage = (after: string | null) => {
    repositoryFocusPending.current = after
    setRepositoryAfter(after)
  }
  const nextActivityPage = () => {
    const page = activity.data
    if (!page) return
    const event = page.event_continuation_before
    const nextWindow: RepoWatchActivityWindow = {
      eventBefore: event
        ? { cursorGeneration: event.cursor_generation, eventOrdinal: event.event_ordinal }
        : undefined,
      webhookBeforeReceiptSequence: page.webhook_continuation_before_receipt_sequence ?? undefined,
      includeEvents: event !== null,
      includeWebhooks: page.webhook_continuation_before_receipt_sequence !== null,
    }
    setActivityPaging(true)
    setIncludeEvents(nextWindow.includeEvents)
    setIncludeWebhooks(nextWindow.includeWebhooks)
    setEventBefore(nextWindow.eventBefore)
    setWebhookBefore(nextWindow.webhookBeforeReceiptSequence)
  }

  return (
    <div className="surface-body activity-surface">
      <div className="activity-toolbar" role="toolbar" aria-label="Repository activity controls">
        <label>
          Repository
          <select
            ref={repositorySelectFocus}
            value={repository ?? ''}
            onChange={(event) => chooseRepository(event.target.value)}
          >
            {repositories.data?.repositories.map((item) => (
              <option key={item.repository}>{item.repository}</option>
            ))}
          </select>
        </label>
        <button
          type="button"
          onClick={() =>
            void Promise.all([
              repositories.refetch(),
              pullRequests.refetch(),
              work.refetch(),
              selectedPullRequest === null ? Promise.resolve() : sessions.refetch(),
              activity.refetch(),
            ])
          }
        >
          <RefreshCw aria-hidden="true" /> Refresh bounded reads
        </button>
      </div>

      {repositories.isError && <p role="alert">{errorMessage(repositories.error)}</p>}
      {status && <RepositoryHealth status={status} />}
      {pullRequests.isError && (
        <p role="alert">Pull requests: {errorMessage(pullRequests.error)}</p>
      )}

      {pullRequests.data && repository && (
        <>
          <PullRequestTable
            page={pullRequests.data}
            repository={repository}
            selected={selectedPullRequest}
            onSelect={(number) => {
              setSelectedPullRequest(number)
              setSessionBefore(undefined)
            }}
            headingRef={pullRequestHeadingFocus}
          />
          <div className="activity-page-controls">
            {pullRequestAfter && (
              <button type="button" onClick={() => changePullRequestPage(null)}>
                First PR page
              </button>
            )}
            {pullRequests.data.continuation_after_pull_request && (
              <button
                type="button"
                onClick={() =>
                  changePullRequestPage(pullRequests.data.continuation_after_pull_request ?? null)
                }
              >
                Next PR page <ArrowRight aria-hidden="true" />
              </button>
            )}
          </div>
        </>
      )}

      {selected && <SessionPanel pullRequest={selected} query={sessions} />}
      {sessionBefore && (
        <button type="button" onClick={() => setSessionBefore(undefined)}>
          First session page
        </button>
      )}
      {sessions.data?.continuation_before && (
        <button
          type="button"
          onClick={() =>
            setSessionBefore({
              commissionedAtUnixMilliseconds:
                sessions.data?.continuation_before?.commissioned_at_unix_milliseconds ?? '',
              sessionId: sessions.data?.continuation_before?.session_id ?? '',
            })
          }
        >
          Older PR sessions <ArrowDown aria-hidden="true" />
        </button>
      )}

      {work.isError && <p role="alert">Work: {errorMessage(work.error)}</p>}
      {(heldAfter || obligationAfter) && (
        <div className="activity-page-controls">
          {heldAfter && (
            <button type="button" onClick={() => setHeldAfter(undefined)}>
              First held page
            </button>
          )}
          {obligationAfter && (
            <button type="button" onClick={() => setObligationAfter(undefined)}>
              First queued page
            </button>
          )}
        </div>
      )}
      {work.data && (
        <>
          <WorkTables page={work.data} />
          <div className="activity-page-controls">
            {work.data.held_continuation_after && (
              <button
                type="button"
                onClick={() =>
                  setHeldAfter({
                    heldSinceUnixMilliseconds:
                      work.data?.held_continuation_after?.held_since_unix_milliseconds ?? '',
                    dispatchId: work.data?.held_continuation_after?.dispatch_id ?? '',
                  })
                }
              >
                Next held page
              </button>
            )}
            {work.data.obligation_continuation_after && (
              <button
                type="button"
                onClick={() =>
                  setObligationAfter({
                    owedSinceUnixMilliseconds:
                      work.data?.obligation_continuation_after?.owed_since_unix_milliseconds ?? '',
                    obligationId: work.data?.obligation_continuation_after?.obligation_id ?? '',
                  })
                }
              >
                Next queued page
              </button>
            )}
          </div>
        </>
      )}

      <section className="activity-history" aria-labelledby="activity-history-heading">
        <header>
          <div>
            <span className="eyebrow">Keyset-paged durable history</span>
            <h2 id="activity-history-heading">Events and webhooks</h2>
          </div>
          <span>{rows.length} loaded in browser window</span>
        </header>
        {activity.isError && <p role="alert">Activity history: {errorMessage(activity.error)}</p>}
        <div className="activity-history-controls">
          <label>
            Filter loaded history
            <input value={filter} onChange={(event) => setFilter(event.target.value)} />
          </label>
          <label>
            Local sort
            <select
              value={sort}
              onChange={(event) => setSort(event.target.value as 'newest' | 'oldest')}
            >
              <option value="newest">Newest first</option>
              <option value="oldest">Oldest first</option>
            </select>
          </label>
        </div>
        <HistoryTable rows={rows} />
        <div className="activity-page-controls">
          {activityPaging && (
            <button
              type="button"
              onClick={() => {
                setEventBefore(undefined)
                setWebhookBefore(undefined)
                setIncludeEvents(true)
                setIncludeWebhooks(true)
                setActivityPaging(false)
                setActivityPages([])
              }}
            >
              Return to latest
            </button>
          )}
          {(activity.data?.event_continuation_before ||
            activity.data?.webhook_continuation_before_receipt_sequence) && (
            <button type="button" onClick={nextActivityPage}>
              Load older window <ArrowDown aria-hidden="true" />
            </button>
          )}
        </div>
      </section>

      <div className="activity-page-controls">
        {repositoryAfter && (
          <button type="button" onClick={() => changeRepositoryPage(null)}>
            First repository page
          </button>
        )}
        {repositories.data?.continuation_after_repository && (
          <button
            type="button"
            onClick={() =>
              changeRepositoryPage(repositories.data?.continuation_after_repository ?? null)
            }
          >
            Next repository page
          </button>
        )}
      </div>
    </div>
  )
}
