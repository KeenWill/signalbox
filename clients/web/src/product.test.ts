import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  BootstrapContractError,
  MAX_BOOTSTRAP_RESPONSE_BYTES,
  ProductRequestError,
  productRoutes,
  productSurfaceCacheLabel,
  productSurfaceStates,
  SameOriginProductTransport,
} from './product'
import { webContractBootstrapFixture } from './product.fixture'
import { hasValidSessionTimelineContract } from './session-timeline/model'

// The generated contract is authored in Rust; keep one fixture for every browser test.
const bootstrapFixture = webContractBootstrapFixture

const sessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d'
const previousSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6c'
const attentionFixture = {
  continuation_after_session_id: sessionId,
  cursor: '17',
  summaries: [
    {
      action: 'decide_approval',
      current_turn_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c70',
      goal_block: null,
      judge: { actionable: '2', completed: '7', escalated: '1', failed: '0' },
      last_activity: { kind: 'approval_judge', unix_milliseconds: '1724200000000' },
      session_id: sessionId,
      state: 'awaiting_approval',
    },
  ],
} as const
const attentionUpdateFixture = {
  kind: 'update',
  cursor: '18',
  summaries: [{ ...attentionFixture.summaries[0], state: 'active', action: null }],
} as const
const errorFixture = {
  error: {
    code: 'attention_projection_unavailable',
    kind: 'application',
    message: 'attention projection is not configured',
  },
} as const

const activityFixture = {
  event_continuation_before: { cursor_generation: '8', event_ordinal: 41 },
  events: [
    {
      cursor_generation: '8',
      event_ordinal: 41,
      id: 'event-41',
      kind: 'head_changed',
      observed_at_unix_milliseconds: '1724200000000',
      pull_request: null,
    },
  ],
  webhook_continuation_before_receipt_sequence: null,
  webhooks: [],
} as const

afterEach(() => vi.unstubAllGlobals())

describe('SameOriginProductTransport', () => {
  it('decodes the Rust-authored bootstrap contract', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(webContractBootstrapFixture))),
    )

    const bootstrap = await new SameOriginProductTransport().readBootstrap()

    expect(bootstrap).toEqual(webContractBootstrapFixture)
  })

  it('fails closed when the daemon returns an unknown contract shape', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify({ invented: true }))),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      BootstrapContractError,
    )
  })

  it('rejects malformed JSON as a contract failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('{')),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      'violates the web contract',
    )
  })

  it('rejects a bootstrap response above the fixed byte ceiling', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('x'.repeat(MAX_BOOTSTRAP_RESPONSE_BYTES + 1))),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      'exceeds the byte limit',
    )
  })

  it('rejects bootstrap capabilities that this client cannot honor', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...bootstrapFixture,
              capabilities: { ...bootstrapFixture.capabilities, ndjson_streaming: false },
            }),
          ),
      ),
    )

    const failure = await new SameOriginProductTransport().readBootstrap().catch((error) => error)

    expect(failure).toBeInstanceOf(BootstrapContractError)
    expect(String((failure as Error).cause)).toContain(
      'bootstrap carries an incompatible web contract',
    )
  })

  it('rejects a mismatched bootstrap contract identity', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...bootstrapFixture,
              contract: { name: 'signalbox.other-http', version: '2' },
            }),
          ),
      ),
    )

    const failure = await new SameOriginProductTransport().readBootstrap().catch((error) => error)

    expect(failure).toBeInstanceOf(BootstrapContractError)
    expect(String((failure as Error).cause)).toContain(
      'bootstrap carries an incompatible web contract',
    )
  })

  it('reports an unsuccessful HTTP response without decoding its body', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(null, { status: 503 })),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow('status 503')
  })

  it('classifies a failed fetch as a typed transport error', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => Promise.reject(new TypeError('offline'))),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository'),
    ).rejects.toEqual(
      new ProductRequestError(
        'network_unavailable',
        'transport',
        'the daemon request could not be completed',
      ),
    )
  })

  it('decodes one bounded attention page and preserves its typed continuation', async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify(attentionFixture)))
    vi.stubGlobal('fetch', fetchMock)

    const snapshot = await new SameOriginProductTransport().readAttention(previousSessionId)

    expect(snapshot).toEqual(attentionFixture)
    expect(fetchMock).toHaveBeenCalledWith(
      `/api/attention?after_session_id=${previousSessionId}`,
      expect.objectContaining({ credentials: 'same-origin' }),
    )
  })

  it('rejects a non-advancing attention page', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(attentionFixture))),
    )

    await expect(new SameOriginProductTransport().readAttention(sessionId)).rejects.toThrow(
      'attention page does not advance beyond the requested cursor',
    )
  })

  it('rejects an attention continuation beyond the returned page boundary', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...attentionFixture,
              continuation_after_session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6e',
            }),
          ),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention(previousSessionId)).rejects.toThrow(
      'attention_snapshot.continuation_after_session_id must be the last returned session identity',
    )
  })

  it('preserves a typed attention projection failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(errorFixture), { status: 503 })),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toEqual(
      new ProductRequestError(
        errorFixture.error.code,
        errorFixture.error.kind,
        errorFixture.error.message,
      ),
    )
  })

  it('encodes both fields of an event cursor and an independently exhausted webhook feed', async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify(activityFixture)))
    vi.stubGlobal('fetch', fetchMock)

    const page = await new SameOriginProductTransport().readRepoWatchActivity(
      'example/repository',
      {
        eventBefore: { cursorGeneration: '9', eventOrdinal: 42 },
        includeEvents: true,
        includeWebhooks: false,
      },
    )

    expect(page).toEqual(activityFixture)
    expect(fetchMock).toHaveBeenCalledWith(
      '/api/repository-watch/activity?repository=example%2Frepository&include_events=true&include_webhooks=false&event_before_cursor_generation=9&event_before_ordinal=42',
      expect.objectContaining({ credentials: 'same-origin' }),
    )
  })

  it('rejects non-advancing activity continuations', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...activityFixture,
              event_continuation_before: { cursor_generation: '9', event_ordinal: 42 },
              webhook_continuation_before_receipt_sequence: '7',
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository', {
        eventBefore: { cursorGeneration: '9', eventOrdinal: 42 },
        webhookBeforeReceiptSequence: '7',
        includeEvents: true,
        includeWebhooks: true,
      }),
    ).rejects.toThrow('does not advance to older history')
  })

  it('rejects activity rows outside the requested older window', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...activityFixture,
              event_continuation_before: null,
              events: [
                {
                  cursor_generation: '9',
                  event_ordinal: 42,
                  id: 'event-42',
                  kind: 'head_changed',
                  observed_at_unix_milliseconds: '1724200000000',
                  pull_request: null,
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository', {
        eventBefore: { cursorGeneration: '9', eventOrdinal: 42 },
        includeEvents: true,
        includeWebhooks: false,
      }),
    ).rejects.toThrow('event rows do not advance to older history')
  })

  it('rejects an activity continuation that does not equal the final row', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...activityFixture,
              event_continuation_before: { cursor_generation: '8', event_ordinal: 40 },
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository'),
    ).rejects.toThrow('event continuation does not equal the returned page boundary')
  })

  it('rejects duplicate event identities in one activity page', async () => {
    const event = {
      cursor_generation: '9',
      event_ordinal: 42,
      id: 'event-42',
      kind: 'head_changed',
      observed_at_unix_milliseconds: '1724200000000',
      pull_request: null,
    } as const
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...activityFixture,
              event_continuation_before: null,
              events: [event, { ...event, cursor_generation: '8', event_ordinal: 41 }],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository'),
    ).rejects.toThrow('activity page repeats an event identity')
  })

  it('fails closed when a repository-watch response carries an unknown field', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify({ ...activityFixture, invented: true }))),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository'),
    ).rejects.toThrow('webrepowatchactivitypage.invented must be absent')
  })

  it('rejects an oversized JSON response before parsing it', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(' '.repeat(bootstrapFixture.limits.max_json_body_bytes + 1))),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository'),
    ).rejects.toThrow('JSON response exceeds the contract ceiling')
  })

  it('rejects a pull-request page for a different repository', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              repository: 'outside/repository',
              pull_requests: [],
              continuation_after_pull_request: null,
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchPullRequests('example/repository'),
    ).rejects.toThrow('does not match the requested repository')
  })

  it('rejects a non-advancing repository continuation', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              continuation_after_repository: 'example/repository',
              repositories: [],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchRepositories('example/repository'),
    ).rejects.toThrow('repository continuation does not equal the returned page boundary')
  })

  it('rejects a repeated held-work cursor', async () => {
    const dispatchId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d'
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              held_continuation_after: {
                dispatch_id: dispatchId,
                held_since_unix_microseconds: '1724200000000000',
              },
              held_slots: [],
              obligation_continuation_after: null,
              queued_obligations: [],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchWork('example/repository', {
        dispatchId,
        heldSinceUnixMicroseconds: '1724200000000000',
      }),
    ).rejects.toThrow('held-work continuation does not equal the returned page boundary')
  })

  it('rejects sessions that do not advance to older history', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              continuation_before: null,
              sessions: [
                {
                  attention: attentionFixture.summaries[0],
                  commissioned_at_unix_microseconds: '1724200000000000',
                  purpose: {
                    dispatch_id: 'dispatch-1',
                    kind: 'operator_commission',
                    template: 'review',
                  },
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchPullRequestSessions(
        'example/repository',
        '17',
        { commissionedAtUnixMicroseconds: '1724200000000000', sessionId: previousSessionId },
      ),
    ).rejects.toThrow('session page does not advance to older history')
  })

  it('rejects a non-advancing pull-request continuation', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              repository: 'example/repository',
              pull_requests: [],
              continuation_after_pull_request: '64',
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchPullRequests('example/repository', '64'),
    ).rejects.toThrow('pull-request continuation does not equal the returned page boundary')
  })

  it('accepts the maximum unsigned pull-request cursor', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              repository: 'example/repository',
              pull_requests: [],
              continuation_after_pull_request: null,
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchPullRequests(
        'example/repository',
        '18446744073709551615',
      ),
    ).resolves.toEqual({
      repository: 'example/repository',
      pull_requests: [],
      continuation_after_pull_request: null,
    })
  })

  it('rejects an activity feed beyond its generated page ceiling', async () => {
    const webhook = {
      action_name: 'opened',
      disposition: 'projected',
      event_name: 'pull_request',
      latest_projected_at_unix_milliseconds: '1724200000000',
      projection_count: '1',
      receipt_sequence: '1',
      received_at_unix_milliseconds: '1724200000000',
    } as const
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...activityFixture,
              webhooks: Array.from({ length: 101 }, (_, index) => ({
                ...webhook,
                receipt_sequence: String(index + 1),
              })),
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository'),
    ).rejects.toThrow('at most 100 items')
  })

  it('decodes complete NDJSON attention events without buffering stream history', async () => {
    const body = `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n${JSON.stringify(attentionUpdateFixture)}\n`
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(body)),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).resolves.toEqual({
      done: false,
      value: { kind: 'snapshot', snapshot: attentionFixture },
    })
    await expect(events.next()).resolves.toEqual({ done: false, value: attentionUpdateFixture })
    await expect(events.next()).resolves.toEqual({ done: true, value: undefined })
  })

  it('cancels an abandoned attention response before reconnecting', async () => {
    const cancel = vi.fn()
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(
          new TextEncoder().encode(
            `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n`,
          ),
        )
      },
      cancel,
    })
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(body)),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).resolves.toEqual({
      done: false,
      value: { kind: 'snapshot', snapshot: attentionFixture },
    })
    await events.return?.(undefined)

    expect(cancel).toHaveBeenCalledOnce()
  })

  it('rejects an attention event beyond the advertised NDJSON item ceiling', async () => {
    const body = `${' '.repeat(bootstrapFixture.limits.max_ndjson_item_bytes + 1)}\n`
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(body)),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).rejects.toThrow(
      'attention stream item exceeds the contract ceiling',
    )
  })

  it('rejects a final attention event without its record delimiter', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(attentionUpdateFixture))),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).rejects.toThrow('attention stream ended with an incomplete item')
  })
})

describe('product surface availability', () => {
  it('requires bounded JSON before enabling timeline reads', () => {
    expect(
      hasValidSessionTimelineContract({
        ...webContractBootstrapFixture,
        capabilities: { ...webContractBootstrapFixture.capabilities, bounded_json: false },
      }),
    ).toBe(false)
  })

  it('rejects timeline capability with unusable semantic limits', () => {
    expect(hasValidSessionTimelineContract(webContractBootstrapFixture)).toBe(true)
    expect(
      hasValidSessionTimelineContract({
        ...webContractBootstrapFixture,
        limits: { ...webContractBootstrapFixture.limits, max_timeline_window_items: 0 },
      }),
    ).toBe(false)
    expect(
      hasValidSessionTimelineContract({
        ...webContractBootstrapFixture,
        limits: { ...webContractBootstrapFixture.limits, max_timeline_window_bytes: 65_537 },
      }),
    ).toBe(false)
  })

  it('defines one typed authority state for every product route', () => {
    expect(productSurfaceStates).toHaveProperty(productRoutes[0].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[1].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[2].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[3].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[4].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[5].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[6].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[7].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[8].id)
  })

  it('keeps Settings browser-local instead of implying daemon authority', () => {
    expect(productSurfaceStates.settings).toEqual({
      kind: 'browser-local',
      authority: 'browser preferences',
    })
  })

  it('marks the available Session timeline facts as server-backed', () => {
    expect(productSurfaceStates.sessions).toEqual({
      kind: 'server-backed',
      owningTrack: '#991 session projections',
      facts: ['bounded session descriptors', 'stable-address timeline windows'],
    })
  })

  it('reports cache ownership only for implemented surfaces', () => {
    expect(productSurfaceCacheLabel('sessions')).toBe('Bounded query')
    expect(productSurfaceCacheLabel('settings')).toBe('Local settings')
    expect(productSurfaceCacheLabel('attention')).toBeNull()
  })
})
