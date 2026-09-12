import { useQuery, useQueryClient } from '@tanstack/react-query'
import { Link } from '@tanstack/react-router'
import { ArrowRight, Radio, RefreshCw, X } from 'lucide-react'
import { useEffect, useRef, useState } from 'react'
import { type AttentionSyncPhase, attentionSnapshotsMatch, synchronizeAttention } from './attention'
import type { WebAttentionSnapshot } from './generated/web-contract.mjs'
import { enumLabel, productLabels } from './labels'
import { ProductRequestError, productTransport } from './product'
import { actions, selectApp, useAppDispatch, useAppSelector } from './state'
import './catalog.css'

type AttentionSummary = WebAttentionSnapshot['summaries'][number]

const phaseCopy: Record<AttentionSyncPhase, string> = {
  idle: productLabels.snapshot,
  connecting: 'Connecting…',
  live: productLabels.live,
  resyncing: 'Reconnecting…',
  stale: productLabels.paused,
  failed: productLabels.disconnected,
}

export const activityTime = (unixMilliseconds: string) => {
  const value = Number(unixMilliseconds)
  if (!Number.isSafeInteger(value)) return unixMilliseconds
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return unixMilliseconds
  return new Intl.DateTimeFormat('en-US', {
    dateStyle: 'medium',
    timeStyle: 'short',
    timeZone: 'UTC',
  }).format(date)
}

const queryKey = (after: string | null) => ['production', 'attention', after] as const

export function AttentionSurface({
  registerEscapeHandler,
}: {
  registerEscapeHandler: (handler: (() => boolean) | null) => void
}) {
  const dispatch = useAppDispatch()
  const phase = useAppSelector(selectApp).attentionSync
  const queryClient = useQueryClient()
  const [after, setAfter] = useState<string | null>(null)
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [monitorGeneration, setMonitorGeneration] = useState(0)
  const returnFocus = useRef<HTMLButtonElement>(null)
  const closeFocus = useRef<HTMLButtonElement>(null)
  const errorFocus = useRef<HTMLButtonElement>(null)
  const pageHeading = useRef<HTMLHeadingElement>(null)
  const focusReplacement = useRef(false)
  const focusRevealedList = useRef(false)
  const liveProjection = useRef<WebAttentionSnapshot | undefined>(undefined)
  const pageCursorFloor = useRef<string | null>(null)
  const attention = useQuery({
    queryKey: queryKey(after),
    queryFn: async ({ signal }) => {
      const snapshot = await productTransport.readAttention(after ?? undefined, signal)
      if (
        after !== null &&
        pageCursorFloor.current !== null &&
        BigInt(snapshot.cursor) < BigInt(pageCursorFloor.current)
      ) {
        throw new TypeError('paged attention snapshot cursor regressed')
      }
      if (after !== null) pageCursorFloor.current = snapshot.cursor
      const latestProjection = liveProjection.current
      if (after !== null || !latestProjection) {
        return snapshot
      }
      if (
        latestProjection.cursor === snapshot.cursor &&
        !attentionSnapshotsMatch(latestProjection, snapshot)
      ) {
        throw new TypeError('attention projections diverged at the same cursor')
      }
      return BigInt(latestProjection.cursor) > BigInt(snapshot.cursor) ? latestProjection : snapshot
    },
    gcTime: 0,
  })
  const selected = attention.data?.summaries.find((summary) => summary.session_id === selectedId)
  const workbenchClass = selected ? 'attention-workbench inspector-open' : 'attention-workbench'

  useEffect(() => {
    void monitorGeneration
    if (after !== null) {
      dispatch(actions.attentionSyncSet('idle'))
      return
    }
    const controller = new AbortController()
    void synchronizeAttention({
      transport: productTransport,
      signal: controller.signal,
      onPhase: (next) => dispatch(actions.attentionSyncSet(next)),
      onProjection: (snapshot) => {
        const queryProjection = queryClient.getQueryData<WebAttentionSnapshot>(queryKey(null))
        if (
          queryProjection &&
          queryProjection.cursor === snapshot.cursor &&
          !attentionSnapshotsMatch(queryProjection, snapshot)
        ) {
          throw new TypeError('attention projections diverged at the same cursor')
        }
        const projection =
          queryProjection && BigInt(queryProjection.cursor) >= BigInt(snapshot.cursor)
            ? queryProjection
            : snapshot
        liveProjection.current = projection
        queryClient.setQueryData(queryKey(null), projection)
        return { snapshot: projection, accepted: projection === snapshot }
      },
    })
    return () => {
      liveProjection.current = undefined
      controller.abort()
      dispatch(actions.attentionSyncSet('idle'))
    }
  }, [after, dispatch, monitorGeneration, queryClient])

  useEffect(() => {
    const target = selectedId ? closeFocus : focusRevealedList.current ? pageHeading : returnFocus
    focusRevealedList.current = false
    const frame = requestAnimationFrame(() => target.current?.focus())
    return () => cancelAnimationFrame(frame)
  }, [selectedId])

  useEffect(() => {
    if (!selectedId) {
      registerEscapeHandler(null)
      return
    }
    registerEscapeHandler(() => {
      setSelectedId(null)
      return true
    })
    return () => registerEscapeHandler(null)
  }, [registerEscapeHandler, selectedId])

  useEffect(() => {
    if (!focusReplacement.current) return
    const target = attention.data ? pageHeading : attention.isError ? errorFocus : null
    if (!target) return
    focusReplacement.current = false
    const frame = requestAnimationFrame(() => target.current?.focus())
    return () => cancelAnimationFrame(frame)
  }, [attention.data, attention.isError])

  useEffect(() => {
    if (!selectedId || !attention.data || selected) return
    returnFocus.current = null
    focusRevealedList.current = true
    setSelectedId(null)
  }, [attention.data, selected, selectedId])

  const open = (summary: AttentionSummary, button: HTMLButtonElement) => {
    returnFocus.current = button
    setSelectedId(summary.session_id)
  }
  const close = () => setSelectedId(null)
  const nextPage = () => {
    const currentPage = attention.data
    const continuation = currentPage?.continuation_after_session_id
    if (!continuation || !currentPage) return
    pageCursorFloor.current = currentPage.cursor
    focusReplacement.current = true
    setSelectedId(null)
    setAfter(continuation)
  }
  const returnToLivePage = () => {
    pageCursorFloor.current = null
    focusReplacement.current = true
    setSelectedId(null)
    setAfter(null)
  }
  const restartMonitor = () => {
    focusReplacement.current = true
    // Every follow response opens with its own coherent snapshot, so the follower never needs
    // the ordinary read to establish a baseline. Restarting concurrently keeps recovery from
    // depending on an HTTP read that can stay pending: both projections are still arbitrated
    // by cursor, in the query function and in onProjection, whichever settles first.
    setMonitorGeneration((generation) => generation + 1)
    void attention.refetch()
  }
  const monitorCanRestart = phase === 'failed' || phase === 'stale'
  const retryAttention = () => {
    focusReplacement.current = true
    void attention.refetch()
  }

  return (
    <div className="surface-body attention-live-surface">
      <div className="attention-monitor-bar">
        <span className={`attention-monitor phase-${phase}`} aria-live="polite">
          <Radio aria-hidden="true" /> {phaseCopy[phase]}
        </span>
        <button
          type="button"
          onClick={monitorCanRestart ? restartMonitor : () => void attention.refetch()}
        >
          <RefreshCw aria-hidden="true" />
          {monitorCanRestart ? 'Reconnect' : 'Refresh'}
        </button>
      </div>

      {attention.isLoading && <p className="attention-notice">Loading sessions…</p>}
      {attention.isError && (
        <section className="surface-empty" role="alert">
          <div>
            <h2>Attention failed to load</h2>
            <p>
              {attention.error instanceof ProductRequestError
                ? `${attention.error.response.error.code}: ${attention.error.message}`
                : 'Unexpected daemon response.'}
            </p>
            <button ref={errorFocus} type="button" onClick={retryAttention}>
              Retry
            </button>
            {after && (
              <button type="button" onClick={returnToLivePage}>
                First page
              </button>
            )}
          </div>
        </section>
      )}

      {attention.data && (
        <div className={workbenchClass}>
          <section className="attention-list" aria-labelledby="attention-heading">
            <header>
              <div>
                <h2 id="attention-heading" ref={pageHeading} tabIndex={-1}>
                  {attention.data.summaries.length}{' '}
                  {attention.data.summaries.length === 1 ? 'session' : 'sessions'}
                </h2>
              </div>
            </header>
            {attention.data.summaries.length === 0 ? (
              <p className="attention-notice">No sessions</p>
            ) : (
              <ol>
                {attention.data.summaries.map((summary) => (
                  <li key={summary.session_id} className={`attention-${summary.state}`}>
                    <Link
                      className="attention-session-link"
                      to="/$surface"
                      params={{ surface: 'sessions' }}
                      search={{ session: summary.session_id, workspace: true }}
                    >
                      <span className="attention-rail" aria-hidden="true" />
                      <span className="attention-identity">
                        <strong>{enumLabel(summary.state)}</strong>
                        <code>{summary.session_id}</code>
                      </span>
                      <span className="attention-obligation">
                        {summary.action ? enumLabel(summary.action) : '—'}
                      </span>
                      <time>{activityTime(summary.last_activity.unix_milliseconds)}</time>
                      <ArrowRight aria-hidden="true" />
                    </Link>
                    <button
                      className="attention-preview"
                      type="button"
                      aria-label={`${productLabels.preview} ${enumLabel(summary.state)} ${summary.session_id}`}
                      aria-pressed={selectedId === summary.session_id}
                      onClick={(event) => open(summary, event.currentTarget)}
                    >
                      {productLabels.preview}
                    </button>
                  </li>
                ))}
              </ol>
            )}
            <div className="attention-page-controls">
              {after && (
                <button type="button" onClick={returnToLivePage}>
                  First page
                </button>
              )}
              {attention.data.continuation_after_session_id && (
                <button type="button" onClick={nextPage}>
                  Next <ArrowRight aria-hidden="true" />
                </button>
              )}
            </div>
          </section>

          {selected && (
            <aside className="attention-inspector" aria-labelledby="attention-inspector-heading">
              <header>
                <div>
                  <h2 id="attention-inspector-heading">{enumLabel(selected.state)}</h2>
                </div>
                <button
                  ref={closeFocus}
                  type="button"
                  aria-label="Close attention inspector"
                  onClick={close}
                >
                  <X aria-hidden="true" />
                </button>
              </header>
              <Link
                to="/$surface"
                params={{ surface: 'sessions' }}
                search={{ session: selected.session_id, workspace: true }}
              >
                {productLabels.openSession} <ArrowRight aria-hidden="true" />
              </Link>
              <dl>
                <div>
                  <dt>Session</dt>
                  <dd>
                    <code>{selected.session_id}</code>
                  </dd>
                </div>
                <div>
                  <dt>Required action</dt>
                  <dd>{selected.action ? enumLabel(selected.action) : 'None'}</dd>
                </div>
                <div>
                  <dt>Current turn</dt>
                  <dd>{selected.current_turn_id ?? 'None'}</dd>
                </div>
                <div>
                  <dt>Last activity</dt>
                  <dd>{enumLabel(selected.last_activity.kind)}</dd>
                </div>
              </dl>
              {selected.goal_block && (
                <section className="attention-goal-block">
                  <span className="eyebrow">
                    Blocked goal · Generation {selected.goal_block.generation}
                  </span>
                  <strong>{enumLabel(selected.goal_block.reason)}</strong>
                  <p>{selected.goal_block.need_summary}</p>
                </section>
              )}
              <section className="attention-judge" aria-label="Approval outcomes">
                <div>
                  <span>Needs decision</span>
                  <strong>{selected.judge.actionable}</strong>
                </div>
                <div>
                  <span>Completed</span>
                  <strong>{selected.judge.completed}</strong>
                </div>
                <div>
                  <span>Escalated</span>
                  <strong>{selected.judge.escalated}</strong>
                </div>
                <div>
                  <span>Failed</span>
                  <strong>{selected.judge.failed}</strong>
                </div>
              </section>
            </aside>
          )}
        </div>
      )}
    </div>
  )
}
