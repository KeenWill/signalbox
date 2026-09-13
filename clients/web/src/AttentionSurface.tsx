import { useQuery, useQueryClient } from '@tanstack/react-query'
import { Navigate } from '@tanstack/react-router'
import { ArrowRight, Radio, RefreshCw } from 'lucide-react'
import { useEffect, useMemo, useRef, useState } from 'react'
import { type AttentionSyncPhase, attentionSnapshotsMatch, synchronizeAttention } from './attention'
import type { WebAttentionSnapshot } from './generated/web-contract.mjs'
import { enumLabel, productLabels } from './labels'
import { ProductRequestError, productTransport } from './product'
import { actions, selectApp, useAppDispatch, useAppSelector } from './state'
import './catalog.css'
import './session-actions.css'

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
  registerEscapeHandler: _registerEscapeHandler,
}: {
  registerEscapeHandler: (handler: (() => boolean) | null) => void
}) {
  return <Navigate to="/$surface" params={{ surface: 'sessions' }} replace />
}

export function AttentionSessions({
  after,
  onAfterChange,
  returnSessionId,
  onReturnFocusConsumed,
  onSessionOpen,
  onTimelineIds,
}: {
  after: string | null
  onAfterChange: (after: string | null) => void
  returnSessionId?: string
  onReturnFocusConsumed: () => void
  onSessionOpen: (sessionId: string) => void
  onTimelineIds: (ids: readonly string[]) => void
}) {
  const dispatch = useAppDispatch()
  const { attentionSync: phase, selectedTimeline, overlay } = useAppSelector(selectApp)
  const sessionLinks = useRef(new Map<string, HTMLButtonElement>())
  const pendingReturnFocus = useRef(returnSessionId)
  const queryClient = useQueryClient()
  const [monitorGeneration, setMonitorGeneration] = useState(0)
  const errorFocus = useRef<HTMLButtonElement>(null)
  const pageHeading = useRef<HTMLHeadingElement>(null)
  const focusReplacement = useRef(false)
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
  const needingAttention = useMemo(
    () => attention.data?.summaries.filter((summary) => summary.action !== null) ?? [],
    [attention.data],
  )
  useEffect(() => {
    onTimelineIds(needingAttention.map((summary) => summary.session_id))
    return () => onTimelineIds([])
  }, [needingAttention, onTimelineIds])
  useEffect(() => {
    if (selectedTimeline && overlay === null) sessionLinks.current.get(selectedTimeline)?.focus()
  }, [selectedTimeline, overlay])
  useEffect(() => {
    if (!attention.data || overlay !== null || !pendingReturnFocus.current) return
    const target = sessionLinks.current.get(pendingReturnFocus.current)
    pendingReturnFocus.current = undefined
    target?.focus()
    onReturnFocusConsumed()
  }, [attention.data, onReturnFocusConsumed, overlay])

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
    if (!focusReplacement.current) return
    const target = attention.data ? pageHeading : attention.isError ? errorFocus : null
    if (!target) return
    focusReplacement.current = false
    const frame = requestAnimationFrame(() => target.current?.focus())
    return () => cancelAnimationFrame(frame)
  }, [attention.data, attention.isError])

  const nextPage = () => {
    const currentPage = attention.data
    const continuation = currentPage?.continuation_after_session_id
    if (!continuation || !currentPage) return
    pageCursorFloor.current = currentPage.cursor
    focusReplacement.current = true
    onAfterChange(continuation)
  }
  const returnToLivePage = () => {
    pageCursorFloor.current = null
    focusReplacement.current = true
    onAfterChange(null)
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
        <div className="attention-workbench">
          <section className="attention-list" aria-labelledby="attention-heading">
            <header>
              <div>
                <h2 id="attention-heading" ref={pageHeading} tabIndex={-1}>
                  {needingAttention.length}{' '}
                  {needingAttention.length === 1 ? 'session needs' : 'sessions need'} attention on
                  this page
                </h2>
              </div>
            </header>
            {needingAttention.length === 0 ? (
              <p className="attention-notice">No sessions need attention on this page</p>
            ) : (
              <ol>
                {needingAttention.map((summary) => (
                  <li key={summary.session_id} className={`attention-${summary.state}`}>
                    <button
                      type="button"
                      className="attention-session-link"
                      ref={(link) => {
                        if (link) sessionLinks.current.set(summary.session_id, link)
                        else sessionLinks.current.delete(summary.session_id)
                      }}
                      onFocus={() => dispatch(actions.timelineSelected(summary.session_id))}
                      onClick={() => onSessionOpen(summary.session_id)}
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
                    </button>
                    <section className="attention-judge" aria-label="Approval outcomes">
                      <span>
                        Needs decision <strong>{summary.judge.actionable}</strong>
                      </span>
                      <span>
                        Completed <strong>{summary.judge.completed}</strong>
                      </span>
                      <span>
                        Escalated <strong>{summary.judge.escalated}</strong>
                      </span>
                      <span>
                        Failed <strong>{summary.judge.failed}</strong>
                      </span>
                    </section>
                    {summary.goal_block && <p>{summary.goal_block.need_summary}</p>}
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
        </div>
      )}
    </div>
  )
}
