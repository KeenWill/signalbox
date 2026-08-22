import { useQuery } from '@tanstack/react-query'
import { ChevronDown, ChevronRight, Radio, SkipBack, SkipForward } from 'lucide-react'
import { type FormEvent, useCallback, useEffect, useMemo, useState } from 'react'
import type {
  WebSessionTimelineDetailPage,
  WebSessionTimelineWindow,
} from './generated/web-contract.mjs'
import {
  assertBoundedDetailPage,
  HttpSessionTimelineSource,
  retainBoundedDetailPages,
  type SessionDetailContinuation,
  type SessionDetailLimits,
  type SessionWindowAnchor,
} from './session-timeline/model'
import { actions, selectApp, useAppDispatch, useAppSelector } from './state'

const SESSION_ID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i
const SESSION_WINDOW_ITEMS = 80
const SESSION_WINDOW_BYTES = 64 * 1024
const ITEM_DETAIL_LIMITS: SessionDetailLimits = { maxItems: 1, maxBytes: 32 * 1024 }
const GROUP_DETAIL_LIMITS: SessionDetailLimits = { maxItems: 24, maxBytes: 64 * 1024 }

type TimelineDetail = WebSessionTimelineDetailPage['items'][number]
type TimelineBody = TimelineDetail['body']
type TextExcerpt = Extract<TimelineBody, { type: 'user_input' }>['text']

export const isCanonicalSessionId = (value: string): boolean => SESSION_ID_PATTERN.test(value)

export const visibleSessionItems = (
  items: WebSessionTimelineWindow['items'],
  detail: 'full' | 'condensed' | 'results',
) =>
  detail === 'results'
    ? items.filter((item) =>
        ['turn_completed', 'turn_failed', 'turn_refused', 'turn_cancelled'].includes(item.kind),
      )
    : items

const humanize = (value: string) => value.replaceAll('_', ' ')

const DetailFacts = ({ facts }: { facts: ReadonlyArray<readonly [string, unknown]> }) => (
  <dl className="timeline-detail-facts">
    {facts.map(([label, value]) => (
      <div key={label}>
        <dt>{label}</dt>
        <dd>{value === null || value === undefined || value === '' ? '—' : String(value)}</dd>
      </div>
    ))}
  </dl>
)

const Excerpt = ({ value, label }: { value: TextExcerpt; label: string }) => (
  <section className="timeline-excerpt" aria-label={label}>
    <header>
      <strong>{label}</strong>
      <small>
        bytes {value.offset_bytes} / {value.total_bytes}
        {value.continuation ? ' · continued' : ' · complete'}
      </small>
    </header>
    <pre>{value.text}</pre>
  </section>
)

const optionalExcerpt = (value: TextExcerpt | null | undefined, label: string) =>
  value ? <Excerpt value={value} label={label} /> : null

const bodyTurnId = (body: TimelineBody): string | null =>
  'turn_id' in body && typeof body.turn_id === 'string' ? body.turn_id : null

function TimelineBodyView({ body }: { body: TimelineBody }) {
  switch (body.type) {
    case 'session_created':
      return body.imported_evidence ? (
        <DetailFacts
          facts={[
            ['Imported entry', body.imported_evidence.imported_entry_id],
            ['Imported position', body.imported_evidence.imported_position],
          ]}
        />
      ) : (
        <p>Native session creation.</p>
      )
    case 'model_settings':
      return (
        <DetailFacts
          facts={[
            ['Turn', body.turn_id],
            ['Cause', body.cause_code],
          ]}
        />
      )
    case 'user_input':
      return (
        <>
          <DetailFacts
            facts={[
              ['Turn', body.turn_id],
              ['Attachments', body.attachments.length],
            ]}
          />
          <Excerpt value={body.text} label="User input" />
          {body.attachments.map((attachment) => (
            <DetailFacts
              key={attachment.blob_id}
              facts={[
                ['Blob reference', attachment.blob_id],
                ['Referenced bytes', attachment.length_bytes],
                ['Media type', attachment.media_type],
              ]}
            />
          ))}
        </>
      )
    case 'model_call':
      return (
        <>
          <DetailFacts
            facts={[
              ['Turn', body.turn_id],
              ['Model call', body.model_call_id],
              ['Model identity', body.model_identity_id],
              ['Request context items', body.request_context_items],
              [
                'State',
                body.state.type === 'terminal'
                  ? `terminal · ${humanize(body.state.disposition)}`
                  : humanize(body.state.type),
              ],
              ['Cause', body.cause_code],
              ['Input tokens', body.usage.input_tokens],
              ['Output tokens', body.usage.output_tokens],
              ['Cache read tokens', body.usage.cache_read_input_tokens],
            ]}
          />
          {optionalExcerpt(body.response, 'Model response')}
        </>
      )
    case 'tool_batch':
      return (
        <>
          <DetailFacts
            facts={[
              ['Turn', body.turn_id],
              ['Producing model call', body.producing_model_call_id],
              ['State', humanize(body.state)],
            ]}
          />
          {body.tools.map((tool) => (
            <section className="timeline-tool" key={tool.request_id}>
              <h4>{tool.tool_name}</h4>
              <DetailFacts
                facts={[
                  ['Request', tool.request_id],
                  ['Attempt', tool.attempt_id],
                  ['State', tool.state ? humanize(tool.state) : null],
                  ['Approval posture', humanize(tool.approval_posture)],
                  ['Effect posture', tool.effect_posture],
                  ['Sandbox posture', tool.sandbox_posture],
                  ['Judge escalated', tool.approval_judge_escalated],
                  ['Operator required', tool.operator_required],
                  ['Cause', tool.cause_code],
                ]}
              />
              {optionalExcerpt(tool.arguments, 'Arguments')}
              {optionalExcerpt(tool.result, 'Result')}
              {optionalExcerpt(tool.failure, 'Failure')}
            </section>
          ))}
          {body.goal_events.map((event) => (
            <DetailFacts
              key={`${event.generation}:${event.event_kind}`}
              facts={[
                ['Goal generation', event.generation],
                ['Goal event', humanize(event.event_kind)],
                ['Reason / need', event.reason],
              ]}
            />
          ))}
        </>
      )
    case 'tool_approval_decision':
      return (
        <>
          <DetailFacts
            facts={[
              ['Turn', body.turn_id],
              ['Tool', body.tool_name],
              ['Decision', humanize(body.decision)],
              ['Source', body.source],
              ['Judge escalated', body.approval_judge_escalated],
            ]}
          />
          {optionalExcerpt(body.rationale, 'Decision rationale')}
        </>
      )
    case 'goal_event':
      return (
        <>
          <DetailFacts
            facts={[
              ['Turn', body.turn_id],
              ['Generation', body.event.generation],
              ['Event', humanize(body.event.event_kind)],
              ['Reason / need', body.event.reason],
            ]}
          />
          {optionalExcerpt(body.event.text, 'Goal detail')}
        </>
      )
    case 'context_compaction':
      return (
        <>
          <DetailFacts
            facts={[
              ['Compaction', body.compaction_id],
              ['Model call', body.model_call_id],
              ['Through position', body.through_position],
              ['Result frontier', body.result_frontier_id],
            ]}
          />
          <Excerpt value={body.summary} label="Compaction summary" />
        </>
      )
    case 'turn_lifecycle':
      return (
        <DetailFacts
          facts={[
            ['Turn', body.turn_id],
            ['Lifecycle', humanize(body.lifecycle)],
            ['Cause', body.cause_code],
          ]}
        />
      )
    case 'reconciliation':
      return (
        <DetailFacts
          facts={[
            ['Turn', body.turn_id],
            ['Operation', `${humanize(body.operation_kind)} · ${body.operation_id}`],
            ['Attempts', body.attempt_count],
            ['Exhausted', body.exhausted],
            ['Operator required', body.operator_required],
            ['Cause', body.cause_code],
          ]}
        />
      )
    case 'runner':
      return (
        <DetailFacts
          facts={[
            ['Runner', body.runner_id],
            ['Placement revision', body.placement_revision],
            ['State', humanize(body.state)],
            ['Sandbox posture', body.sandbox_posture],
            ['Working directory', body.working_directory],
          ]}
        />
      )
    case 'delegation':
      return (
        <>
          <DetailFacts
            facts={[
              ['Event', humanize(body.event_kind)],
              ['Relationship', body.relationship_id],
              ['Subject', body.subject_id],
              ['Outcome', body.outcome],
              ['Reason', body.reason],
            ]}
          />
          {optionalExcerpt(body.content, 'Delegated or imported evidence')}
        </>
      )
  }
}

function TimelineDetailCard({ detail }: { detail: TimelineDetail }) {
  return (
    <article className="timeline-detail-card">
      <header>
        <span className="session-address">{detail.address.event_sequence}</span>
        <strong>{humanize(detail.kind)}</strong>
        <small>{detail.projected_body_bytes} B body</small>
      </header>
      <TimelineBodyView body={detail.body} />
    </article>
  )
}

export function SessionWorkspaceSurface({
  onTimelineIds,
  onToggleReady,
}: {
  onTimelineIds: (ids: readonly string[]) => void
  onToggleReady: (toggle: (() => void) | null) => void
}) {
  const dispatch = useAppDispatch()
  const app = useAppSelector(selectApp)
  const [draftId, setDraftId] = useState('')
  const [sessionId, setSessionId] = useState<string | null>(null)
  const [manualAnchor, setManualAnchor] = useState<SessionWindowAnchor | null>(null)
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set())
  const [turns, setTurns] = useState<ReadonlySet<string>>(new Set())
  const [pages, setPages] = useState<ReadonlyMap<string, WebSessionTimelineDetailPage>>(new Map())
  const [loading, setLoading] = useState<ReadonlySet<string>>(new Set())
  const [errors, setErrors] = useState<ReadonlyMap<string, string>>(new Map())
  const remembered = sessionId === null ? undefined : app.lastLogicalPositions[sessionId]
  const session = useQuery({
    queryKey: ['production', 'session-workspace', sessionId, manualAnchor, remembered],
    queryFn: async ({ signal }) => {
      const source = await HttpSessionTimelineSource.connect(window.fetch.bind(window))
      const descriptor = await source.readDescriptor(sessionId ?? '', signal)
      const active = BigInt(descriptor.work.active_turn_count) !== BigInt(0)
      const anchor: SessionWindowAnchor =
        manualAnchor ??
        (!active && remembered ? { kind: 'around', eventSequence: remembered } : { kind: 'latest' })
      const timelineWindow = await source.readWindow(
        sessionId ?? '',
        anchor,
        { maxItems: SESSION_WINDOW_ITEMS, maxBytes: SESSION_WINDOW_BYTES },
        signal,
      )
      return { active, descriptor, source, window: timelineWindow }
    },
    enabled: sessionId !== null,
  })
  const items = useMemo(
    () => visibleSessionItems(session.data?.window.items ?? [], app.detail),
    [app.detail, session.data?.window.items],
  )
  const timelineIds = useMemo(() => items.map((item) => item.address.event_sequence), [items])

  useEffect(() => onTimelineIds(timelineIds), [onTimelineIds, timelineIds])
  useEffect(() => () => onTimelineIds([]), [onTimelineIds])

  const openSession = (event: FormEvent) => {
    event.preventDefault()
    const candidate = draftId.trim().toLowerCase()
    if (!isCanonicalSessionId(candidate)) return
    setManualAnchor(null)
    setExpanded(new Set())
    setTurns(new Set())
    setPages(new Map())
    setErrors(new Map())
    dispatch(actions.timelineSelected(null))
    setSessionId(candidate)
  }
  const selected = app.selectedTimeline
  const select = (eventSequence: string) => {
    dispatch(actions.timelineSelected(eventSequence))
    if (sessionId !== null) {
      dispatch(actions.logicalPositionRecorded({ sessionId, position: eventSequence }))
    }
  }
  const load = useCallback(
    async (
      key: string,
      limits: SessionDetailLimits,
      request: () => Promise<WebSessionTimelineDetailPage>,
      append = false,
    ) => {
      if (sessionId === null) return
      setLoading((current) => new Set(current).add(key))
      setErrors((current) => {
        const next = new Map(current)
        next.delete(key)
        return next
      })
      try {
        const incoming = assertBoundedDetailPage(sessionId, await request(), limits)
        setPages((current) => {
          const previous = current.get(key)
          const page =
            append && previous
              ? {
                  ...incoming,
                  items: [...previous.items, ...incoming.items],
                  projected_body_bytes:
                    previous.projected_body_bytes + incoming.projected_body_bytes,
                }
              : incoming
          return retainBoundedDetailPages(current, key, page)
        })
      } catch (error) {
        setErrors((current) => new Map(current).set(key, String(error)))
      } finally {
        setLoading((current) => {
          const next = new Set(current)
          next.delete(key)
          return next
        })
      }
    },
    [sessionId],
  )
  const loadItem = useCallback(
    (address: string, continuation: SessionDetailContinuation | null = null) => {
      const source = session.data?.source
      if (!source || sessionId === null) return
      const key = `item:${address}`
      void load(
        key,
        ITEM_DETAIL_LIMITS,
        () => source.readItemDetails(sessionId, address, continuation, ITEM_DETAIL_LIMITS),
        continuation !== null,
      )
    },
    [load, session.data?.source, sessionId],
  )
  const toggleExpanded = useCallback(
    (eventSequence: string) => {
      const isExpanded = expanded.has(eventSequence)
      const next = new Set(expanded)
      if (isExpanded) next.delete(eventSequence)
      else next.add(eventSequence)
      setExpanded(next)
      if (!isExpanded && !pages.has(`item:${eventSequence}`)) loadItem(eventSequence)
    },
    [expanded, loadItem, pages],
  )
  const toggleSelected = useCallback(() => {
    if (selected !== null && timelineIds.includes(selected)) toggleExpanded(selected)
  }, [selected, timelineIds, toggleExpanded])

  useEffect(() => {
    onToggleReady(toggleSelected)
    return () => onToggleReady(null)
  }, [onToggleReady, toggleSelected])

  const toggleTurn = (turnId: string) => {
    const key = `turn:${turnId}`
    const isExpanded = turns.has(turnId)
    const next = new Set(turns)
    if (isExpanded) next.delete(turnId)
    else next.add(turnId)
    setTurns(next)
    const source = session.data?.source
    if (!isExpanded && !pages.has(key) && source && sessionId) {
      void load(key, GROUP_DETAIL_LIMITS, () =>
        source.readTurnDetails(sessionId, turnId, null, GROUP_DETAIL_LIMITS),
      )
    }
  }
  const windowFirst = session.data?.window.items[0]?.address.event_sequence
  const windowThrough = session.data?.window.items.at(-1)?.address.event_sequence
  const regionKey = windowFirst && windowThrough ? `region:${windowFirst}:${windowThrough}` : null
  const loadRegion = (continuation: SessionDetailContinuation | null = null) => {
    const source = session.data?.source
    if (!source || !sessionId || !windowFirst || !windowThrough || !regionKey) return
    void load(
      regionKey,
      GROUP_DETAIL_LIMITS,
      () =>
        source.readRegionDetails(
          sessionId,
          windowFirst,
          windowThrough,
          continuation,
          GROUP_DETAIL_LIMITS,
        ),
      continuation !== null,
    )
  }

  return (
    <div className="surface-body session-workspace-surface">
      <form className="session-open-form" onSubmit={openSession}>
        <label>
          Exact session ID
          <input
            aria-label="Exact session ID"
            placeholder="00000000-0000-0000-0000-000000000000"
            value={draftId}
            onChange={(event) => setDraftId(event.target.value)}
            pattern={SESSION_ID_PATTERN.source}
            required
          />
        </label>
        <button type="submit" disabled={!isCanonicalSessionId(draftId.trim())}>
          Open workspace
        </button>
      </form>

      {sessionId === null ? (
        <section className="surface-empty session-entry" aria-labelledby="session-entry-heading">
          <Radio aria-hidden="true" />
          <div>
            <span className="availability-tag ready">Timeline detail available</span>
            <h2 id="session-entry-heading">Open a known session by immutable identity</h2>
            <p>
              Enter an exact server-issued ID. The workspace reads one bounded window, then only the
              item, turn, or contiguous region you explicitly expand.
            </p>
          </div>
        </section>
      ) : session.isError ? (
        <p className="session-load-state" role="alert">
          The daemon could not provide this bounded session window: {session.error.message}
        </p>
      ) : !session.data ? (
        <p className="session-load-state">Loading descriptor and bounded history…</p>
      ) : (
        <section className="session-workspace" aria-labelledby="session-workspace-heading">
          <header className="session-workspace-header">
            <div>
              <span className="eyebrow">Stable timeline identity</span>
              <h2 id="session-workspace-heading">{sessionId}</h2>
              <p>
                {session.data.active
                  ? 'Active · opened near latest'
                  : 'Inactive · restored logical position'}
              </p>
            </div>
            <dl className="session-telemetry">
              <div>
                <dt>Items</dt>
                <dd>{session.data.descriptor.sizes.item_count}</dd>
              </div>
              <div>
                <dt>Active</dt>
                <dd>{session.data.descriptor.work.active_turn_count}</dd>
              </div>
              <div>
                <dt>Queued</dt>
                <dd>{session.data.descriptor.work.queued_turn_count}</dd>
              </div>
              <div>
                <dt>Observed</dt>
                <dd>{session.data.descriptor.observed_through}</dd>
              </div>
            </dl>
          </header>
          <div className="session-window-controls" role="toolbar" aria-label="Timeline window">
            <button type="button" onClick={() => setManualAnchor({ kind: 'first' })}>
              <SkipBack aria-hidden="true" /> First <kbd>gg</kbd>
            </button>
            <button type="button" onClick={() => setManualAnchor({ kind: 'latest' })}>
              <SkipForward aria-hidden="true" /> Latest <kbd>G</kbd>
            </button>
            <button type="button" onClick={() => loadRegion()} disabled={regionKey === null}>
              Inspect loaded region
            </button>
            <span>
              {session.data.window.items.length} bounded items ·{' '}
              {session.data.window.projected_structured_bytes} B
            </span>
          </div>
          <ol className="session-timeline" aria-label="Session timeline">
            {items.map((item) => {
              const id = item.address.event_sequence
              const key = `item:${id}`
              const isExpanded = expanded.has(id)
              const page = pages.get(key)
              const turnId = page?.items.map((detail) => bodyTurnId(detail.body)).find(Boolean)
              const turnKey = turnId ? `turn:${turnId}` : null
              return (
                <li key={id} className={selected === id ? 'selected' : undefined}>
                  <button
                    type="button"
                    className="session-item-summary"
                    aria-expanded={isExpanded}
                    onClick={() => {
                      select(id)
                      toggleExpanded(id)
                    }}
                  >
                    {isExpanded ? (
                      <ChevronDown aria-hidden="true" />
                    ) : (
                      <ChevronRight aria-hidden="true" />
                    )}
                    <span className="session-address">{id}</span>
                    <strong>{humanize(item.kind)}</strong>
                    <small>{item.projected_structured_bytes} B header</small>
                  </button>
                  {isExpanded && (
                    <div className="session-item-detail">
                      {loading.has(key) && <p role="status">Loading one bounded item detail…</p>}
                      {errors.has(key) && <p role="alert">{errors.get(key)}</p>}
                      {page?.items.map((detail) => (
                        <TimelineDetailCard
                          key={`${detail.address.event_sequence}:${detail.body.type}:${detail.projected_body_bytes}`}
                          detail={detail}
                        />
                      ))}
                      {page?.continuation && (
                        <button
                          type="button"
                          onClick={() => loadItem(id, page.continuation ?? null)}
                        >
                          Continue bounded item body
                        </button>
                      )}
                      {turnId && turnKey && (
                        <section className="timeline-turn-detail">
                          <button
                            type="button"
                            aria-expanded={turns.has(turnId)}
                            onClick={() => toggleTurn(turnId)}
                          >
                            {turns.has(turnId) ? 'Collapse turn' : 'Expand turn'} {turnId}
                          </button>
                          {turns.has(turnId) && (
                            <>
                              {loading.has(turnKey) && <p role="status">Loading bounded turn…</p>}
                              {pages.get(turnKey)?.items.map((detail) => (
                                <TimelineDetailCard
                                  key={`${detail.address.event_sequence}:${detail.body.type}:${detail.projected_body_bytes}`}
                                  detail={detail}
                                />
                              ))}
                              {pages.get(turnKey)?.continuation && (
                                <button
                                  type="button"
                                  onClick={() => {
                                    const continuation = pages.get(turnKey)?.continuation ?? null
                                    if (continuation && sessionId) {
                                      void load(
                                        turnKey,
                                        GROUP_DETAIL_LIMITS,
                                        () =>
                                          session.data.source.readTurnDetails(
                                            sessionId,
                                            turnId,
                                            continuation,
                                            GROUP_DETAIL_LIMITS,
                                          ),
                                        true,
                                      )
                                    }
                                  }}
                                >
                                  Continue bounded turn
                                </button>
                              )}
                            </>
                          )}
                        </section>
                      )}
                    </div>
                  )}
                </li>
              )
            })}
          </ol>
          {regionKey && pages.get(regionKey) && (
            <section className="timeline-region" aria-labelledby="timeline-region-heading">
              <header>
                <div>
                  <span className="eyebrow">Contiguous bounded region</span>
                  <h3 id="timeline-region-heading">
                    Addresses {windowFirst}–{windowThrough}
                  </h3>
                </div>
                <small>{pages.get(regionKey)?.items.length} retained details</small>
              </header>
              {pages.get(regionKey)?.items.map((detail) => (
                <TimelineDetailCard
                  key={`${detail.address.event_sequence}:${detail.body.type}:${detail.projected_body_bytes}`}
                  detail={detail}
                />
              ))}
              {pages.get(regionKey)?.continuation && (
                <button
                  type="button"
                  disabled={loading.has(regionKey)}
                  onClick={() => loadRegion(pages.get(regionKey)?.continuation ?? null)}
                >
                  {loading.has(regionKey) ? 'Loading next bounded page…' : 'Continue region'}
                </button>
              )}
            </section>
          )}
        </section>
      )}
    </div>
  )
}
