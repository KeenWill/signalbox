import { useQuery, useQueryClient } from '@tanstack/react-query'
import { ArrowRight, Radio, RefreshCw, X } from 'lucide-react'
import { useEffect, useRef, useState } from 'react'
import { type AttentionSyncPhase, synchronizeAttention } from './attention'
import type { WebAttentionSnapshot } from './generated/web-contract.mjs'
import { ProductRequestError, productTransport } from './product'
import { actions, selectApp, useAppDispatch, useAppSelector } from './state'

type AttentionSummary = WebAttentionSnapshot['summaries'][number]

const phaseCopy: Record<AttentionSyncPhase, string> = {
  idle: 'Paged snapshot',
  connecting: 'Connecting monitor',
  live: 'Live monitor',
  resyncing: 'Resynchronizing',
  stale: 'Monitor paused',
  failed: 'Monitor unavailable',
}

const label = (value: string) => value.replaceAll('_', ' ')

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
  const attention = useQuery({
    queryKey: queryKey(after),
    queryFn: async ({ signal }) => {
      const projectionAtStart = liveProjection.current
      const snapshot = await productTransport.readAttention(after ?? undefined, signal)
      const latestProjection = liveProjection.current
      if (after !== null || latestProjection === projectionAtStart || !latestProjection) {
        return snapshot
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
    const continuation = attention.data?.continuation_after_session_id
    if (!continuation) return
    focusReplacement.current = true
    setSelectedId(null)
    setAfter(continuation)
  }
  const returnToLivePage = () => {
    focusReplacement.current = true
    setSelectedId(null)
    setAfter(null)
  }
  const restartMonitor = () => {
    void attention.refetch().then(() => {
      setMonitorGeneration((generation) => generation + 1)
    })
  }
  const monitorCanRestart = phase === 'failed' || phase === 'stale'

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
          {phase === 'failed' ? 'Restart monitor' : 'Refresh snapshot'}
        </button>
      </div>

      {attention.isLoading && <p className="attention-notice">Reading one bounded fleet page…</p>}
      {attention.isError && (
        <section className="surface-empty" role="alert">
          <div>
            <h2>Attention could not be read</h2>
            <p>
              {attention.error instanceof ProductRequestError
                ? `${attention.error.code}: ${attention.error.message}`
                : 'The response did not match the generated web contract.'}
            </p>
            <button ref={errorFocus} type="button" onClick={() => void attention.refetch()}>
              Retry
            </button>
            {after && (
              <button type="button" onClick={returnToLivePage}>
                Return to live page
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
                <span className="eyebrow">Bounded intervention fleet</span>
                <h2 id="attention-heading" ref={pageHeading} tabIndex={-1}>
                  {attention.data.summaries.length} sessions
                </h2>
              </div>
              <code>cursor {attention.data.cursor}</code>
            </header>
            {attention.data.summaries.length === 0 ? (
              <p className="attention-notice">No sessions occupy this fleet page.</p>
            ) : (
              <ol>
                {attention.data.summaries.map((summary) => (
                  <li key={summary.session_id} className={`attention-${summary.state}`}>
                    <button
                      type="button"
                      aria-pressed={selectedId === summary.session_id}
                      onClick={(event) => open(summary, event.currentTarget)}
                    >
                      <span className="attention-rail" aria-hidden="true" />
                      <span className="attention-identity">
                        <strong>{label(summary.state)}</strong>
                        <code>{summary.session_id}</code>
                      </span>
                      <span className="attention-obligation">
                        {summary.action ? label(summary.action) : 'Observe only'}
                      </span>
                      <span>{summary.current_turn_id ?? 'No current turn'}</span>
                      <time>{activityTime(summary.last_activity.unix_milliseconds)}</time>
                      <ArrowRight aria-hidden="true" />
                    </button>
                  </li>
                ))}
              </ol>
            )}
            <div className="attention-page-controls">
              {after && (
                <button type="button" onClick={returnToLivePage}>
                  Return to live page
                </button>
              )}
              {attention.data.continuation_after_session_id && (
                <button type="button" onClick={nextPage}>
                  Next page <ArrowRight aria-hidden="true" />
                </button>
              )}
            </div>
          </section>

          {selected && (
            <aside className="attention-inspector" aria-labelledby="attention-inspector-heading">
              <header>
                <div>
                  <span className="eyebrow">Current obligation</span>
                  <h2 id="attention-inspector-heading">{label(selected.state)}</h2>
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
              <dl>
                <div>
                  <dt>Session</dt>
                  <dd>
                    <code>{selected.session_id}</code>
                  </dd>
                </div>
                <div>
                  <dt>Required action</dt>
                  <dd>{selected.action ? label(selected.action) : 'None'}</dd>
                </div>
                <div>
                  <dt>Current turn</dt>
                  <dd>{selected.current_turn_id ?? 'None'}</dd>
                </div>
                <div>
                  <dt>Activity source</dt>
                  <dd>{label(selected.last_activity.kind)}</dd>
                </div>
              </dl>
              {selected.goal_block && (
                <section className="attention-goal-block">
                  <span className="eyebrow">
                    Blocked goal · generation {selected.goal_block.generation}
                  </span>
                  <strong>{label(selected.goal_block.reason)}</strong>
                  <p>{selected.goal_block.need_summary}</p>
                </section>
              )}
              <section className="attention-judge" aria-label="Approval judgment outcomes">
                <div>
                  <span>Actionable</span>
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
              <p className="attention-readonly-note">
                This projection names the owed action. Its mutation remains on the owning session or
                review surface.
              </p>
            </aside>
          )}
        </div>
      )}
    </div>
  )
}
