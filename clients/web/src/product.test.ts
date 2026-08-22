import { afterEach, describe, expect, it, vi } from 'vitest'
import { ProductRequestError, SameOriginProductTransport } from './product'

const bootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '1' },
  capabilities: {
    bounded_json: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
  },
  limits: { max_json_body_bytes: 65_536, max_ndjson_item_bytes: 65_536 },
} as const

const sessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d'
const attentionFixture = {
  continuation_after_session_id: sessionId,
  cursor: '17',
  summaries: [
    {
      action: 'decide_approval',
      current_turn_id: 'turn-31',
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
  events: [],
  webhook_continuation_before_receipt_sequence: null,
  webhooks: [],
} as const

afterEach(() => vi.unstubAllGlobals())

describe('SameOriginProductTransport', () => {
  it('decodes the Rust-authored bootstrap contract', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(bootstrapFixture))),
    )

    const bootstrap = await new SameOriginProductTransport().readBootstrap()

    expect(bootstrap).toEqual(bootstrapFixture)
  })

  it('fails closed when the daemon returns an unknown contract shape', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify({ invented: true }))),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      'bootstrap.contract',
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

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      'capabilities or limits are incompatible',
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

    const snapshot = await new SameOriginProductTransport().readAttention(sessionId)

    expect(snapshot).toEqual(attentionFixture)
    expect(fetchMock).toHaveBeenCalledWith(
      `/api/attention?after_session_id=${sessionId}`,
      expect.objectContaining({ credentials: 'same-origin' }),
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

  it('fails closed when a repository-watch response carries an unknown field', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify({ ...activityFixture, invented: true }))),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository'),
    ).rejects.toThrow('activity_page')
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
