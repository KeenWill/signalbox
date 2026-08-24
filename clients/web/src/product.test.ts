import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  MAX_ATTENTION_SNAPSHOT_BYTES,
  MAX_ATTENTION_SNAPSHOT_ITEMS,
  MAX_BOOTSTRAP_BYTES,
  ProductRequestError,
  SameOriginProductTransport,
} from './product'

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
const laterSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6e'
const turnId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c70'
const attentionFixture = {
  continuation_after_session_id: null,
  cursor: '17',
  summaries: [
    {
      action: 'decide_approval',
      current_turn_id: turnId,
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

  it('rejects bootstrap values outside the exact supported contract', async () => {
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
      'bootstrap carries an incompatible web contract',
    )
  })

  it('rejects bootstrap before buffering beyond the JSON byte ceiling', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(' '.repeat(MAX_BOOTSTRAP_BYTES + 1))),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      'bootstrap response exceeds the contract byte ceiling',
    )
  })

  it('reports an unsuccessful HTTP response without decoding its body', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(null, { status: 503 })),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow('status 503')
  })

  it('cancels an unsuccessful bootstrap response body', async () => {
    let cancelled = false
    const body = new ReadableStream<Uint8Array>({
      cancel() {
        cancelled = true
      },
    })
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(body, { status: 503 })),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow('status 503')
    expect(cancelled).toBe(true)
  })

  it('decodes one bounded attention page and preserves its typed continuation', async () => {
    const summaries = Array.from({ length: MAX_ATTENTION_SNAPSHOT_ITEMS }, (_, index) => ({
      ...attentionFixture.summaries[0],
      session_id: `018f1840-6f3d-7a8b-9c1d-${(
        BigInt(`0x${sessionId.slice(-12)}`) + BigInt(index + 1)
      )
        .toString(16)
        .padStart(12, '0')}`,
    }))
    const pagedFixture = {
      ...attentionFixture,
      continuation_after_session_id: summaries.at(-1)?.session_id,
      summaries,
    }
    const fetchMock = vi.fn(async () => new Response(JSON.stringify(pagedFixture)))
    vi.stubGlobal('fetch', fetchMock)

    const snapshot = await new SameOriginProductTransport().readAttention(sessionId)

    expect(snapshot).toEqual(pagedFixture)
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

  it('classifies a rejected attention fetch as a transport failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => Promise.reject(new TypeError('Failed to fetch'))),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toEqual(
      new ProductRequestError(
        'transport_unavailable',
        'transport',
        'Network request failed before a response was received.',
      ),
    )
  })

  it('rejects a typed error before buffering beyond the JSON byte ceiling', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(' '.repeat(MAX_ATTENTION_SNAPSHOT_BYTES + 1), { status: 503 }),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'error response exceeds the contract byte ceiling',
    )
  })

  it('decodes complete NDJSON attention events without buffering stream history', async () => {
    const body = `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n${JSON.stringify(attentionUpdateFixture)}\n`
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(body, { headers: { 'content-type': 'application/x-ndjson' } }),
      ),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).resolves.toEqual({
      done: false,
      value: { kind: 'snapshot', snapshot: attentionFixture },
    })
    await expect(events.next()).resolves.toEqual({ done: false, value: attentionUpdateFixture })
    await expect(events.next()).resolves.toEqual({ done: true, value: undefined })
  })

  it('rejects a successful follow response with the wrong media type', async () => {
    const body = `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n`
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(body, { headers: { 'content-type': 'application/json' } })),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).rejects.toThrow(
      'attention stream response must use application/x-ndjson',
    )
  })

  it('rejects malformed cursors in HTTP snapshots and stream events', async () => {
    const malformedSnapshot = { ...attentionFixture, cursor: '01' }
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(new Response(JSON.stringify(malformedSnapshot)))
      .mockResolvedValueOnce(
        new Response(`${JSON.stringify({ ...attentionUpdateFixture, cursor: 'not-a-number' })}\n`, {
          headers: { 'content-type': 'application/x-ndjson' },
        }),
      )
    vi.stubGlobal('fetch', fetchMock)

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention_snapshot.cursor must be a string matching',
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()
    await expect(events.next()).rejects.toThrow('attention_event must be one recognized variant')
  })

  it('rejects an attention snapshot beyond the contract item ceiling', async () => {
    const summaries = Array.from({ length: MAX_ATTENTION_SNAPSHOT_ITEMS + 1 }, (_, index) => ({
      ...attentionFixture.summaries[0],
      session_id: `${sessionId}-${index}`,
    }))
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify({ ...attentionFixture, summaries }))),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      `attention_snapshot.summaries must be at most ${MAX_ATTENTION_SNAPSHOT_ITEMS} items`,
    )
  })

  it('rejects duplicate session identities in HTTP attention snapshots', async () => {
    const duplicate = {
      ...attentionFixture,
      summaries: [attentionFixture.summaries[0], attentionFixture.summaries[0]],
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(duplicate))),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention snapshot contains duplicate session identities',
    )
  })

  it('rejects duplicate session identities in streamed attention snapshots', async () => {
    const duplicate = {
      ...attentionFixture,
      summaries: [attentionFixture.summaries[0], attentionFixture.summaries[0]],
    }
    const body = `${JSON.stringify({ kind: 'snapshot', snapshot: duplicate })}\n`
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(body, { headers: { 'content-type': 'application/x-ndjson' } }),
      ),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).rejects.toThrow(
      'attention snapshot contains duplicate session identities',
    )
  })

  it('rejects incoherent state and action pairs in snapshots and updates', async () => {
    const incoherentSummary = {
      ...attentionFixture.summaries[0],
      state: 'idle',
      action: 'restore_runner',
    }
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ ...attentionFixture, summaries: [incoherentSummary] })),
      )
      .mockResolvedValueOnce(
        new Response(
          `${JSON.stringify({ kind: 'update', cursor: '18', summaries: [incoherentSummary] })}\n`,
          { headers: { 'content-type': 'application/x-ndjson' } },
        ),
      )
    vi.stubGlobal('fetch', fetchMock)

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention_snapshot.summaries[0].action must be consistent with attention state "idle"',
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()
    await expect(events.next()).rejects.toThrow(
      'attention_event.summaries[0].action must be consistent with attention state "idle"',
    )
  })

  it('accepts an actionless approval wait', async () => {
    const actionless = { ...attentionFixture.summaries[0], action: null }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(JSON.stringify({ ...attentionFixture, summaries: [actionless] })),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).resolves.toEqual({
      ...attentionFixture,
      summaries: [actionless],
    })
  })

  it('accepts an actionless runner-loss summary', async () => {
    const runnerLost = {
      ...attentionFixture.summaries[0],
      action: null,
      state: 'runner_lost',
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(JSON.stringify({ ...attentionFixture, summaries: [runnerLost] })),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).resolves.toEqual({
      ...attentionFixture,
      summaries: [runnerLost],
    })
  })

  it('accepts an actionless tool-recovery summary', async () => {
    const awaitingToolRecovery = {
      ...attentionFixture.summaries[0],
      action: null,
      state: 'awaiting_tool_recovery',
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify({ ...attentionFixture, summaries: [awaitingToolRecovery] })),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).resolves.toEqual({
      ...attentionFixture,
      summaries: [awaitingToolRecovery],
    })
  })

  it('accepts an actionless reconciliation summary', async () => {
    const awaitingReconciliation = {
      ...attentionFixture.summaries[0],
      action: null,
      state: 'awaiting_reconciliation',
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({ ...attentionFixture, summaries: [awaitingReconciliation] }),
          ),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).resolves.toEqual({
      ...attentionFixture,
      summaries: [awaitingReconciliation],
    })
  })

  it('rejects turn-derived states without a current-turn identity', async () => {
    const withoutTurn = {
      ...attentionFixture.summaries[0],
      action: null,
      current_turn_id: null,
      state: 'active',
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(JSON.stringify({ ...attentionFixture, summaries: [withoutTurn] })),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'turn-derived attention summary must include a current-turn identity',
    )
  })

  it('rejects a blocked summary without goal-block evidence', async () => {
    const blockedWithoutEvidence = {
      ...attentionFixture.summaries[0],
      action: 'provide_goal_need',
      goal_block: null,
      state: 'blocked',
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({ ...attentionFixture, summaries: [blockedWithoutEvidence] }),
          ),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'blocked attention summary must include goal-block evidence',
    )
  })

  it('rejects malformed session identities and judge counts', async () => {
    const malformedIdentity = { ...attentionFixture.summaries[0], session_id: 'not-a-uuid' }
    const malformedCount = {
      ...attentionFixture.summaries[0],
      judge: { ...attentionFixture.summaries[0].judge, failed: '-1' },
    }
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({
            ...attentionFixture,
            continuation_after_session_id: null,
            summaries: [malformedIdentity],
          }),
        ),
      )
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ ...attentionFixture, summaries: [malformedCount] })),
      )
    vi.stubGlobal('fetch', fetchMock)

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention_snapshot.summaries[0].session_id must be a string matching',
    )
    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention_snapshot.summaries[0].judge.failed must be a string matching',
    )
  })

  it('rejects streamed updates beyond the contract item ceiling', async () => {
    const summaries = Array.from({ length: MAX_ATTENTION_SNAPSHOT_ITEMS + 1 }, (_, index) => ({
      ...attentionFixture.summaries[0],
      session_id: `018f1840-6f3d-7a8b-9c1d-${index.toString(16).padStart(12, '0')}`,
    }))
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(`${JSON.stringify({ kind: 'update', cursor: '18', summaries })}\n`, {
            headers: { 'content-type': 'application/x-ndjson' },
          }),
      ),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).rejects.toThrow('attention_event must be one recognized variant')
  })

  it('rejects attention summaries that are not ordered by session identity', async () => {
    const unordered = {
      ...attentionFixture,
      summaries: [
        { ...attentionFixture.summaries[0], session_id: laterSessionId },
        attentionFixture.summaries[0],
      ],
      continuation_after_session_id: sessionId,
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(unordered))),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention snapshot summaries are not ordered by session identity',
    )
  })

  it('rejects a continuation that does not match the last session identity', async () => {
    const incoherent = { ...attentionFixture, continuation_after_session_id: laterSessionId }
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(incoherent))),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention snapshot continuation does not match its last session identity',
    )
  })

  it('rejects paged summaries at or before the requested keyset cursor', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(attentionFixture))),
    )

    await expect(new SameOriginProductTransport().readAttention(sessionId)).rejects.toThrow(
      'attention snapshot contains an identity at or before its keyset cursor',
    )
  })

  it('accepts an omitted optional continuation', async () => {
    const withoutContinuation = {
      cursor: attentionFixture.cursor,
      summaries: attentionFixture.summaries,
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(withoutContinuation))),
    )

    await expect(new SameOriginProductTransport().readAttention()).resolves.toEqual(
      withoutContinuation,
    )
  })

  it('classifies an attention response-body read failure as a transport failure', async () => {
    const body = new ReadableStream<Uint8Array>({
      pull() {
        throw new TypeError('connection reset')
      },
    })
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(body, { headers: { 'content-type': 'application/x-ndjson' } }),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toEqual(
      new ProductRequestError(
        'transport_unavailable',
        'transport',
        'Network request failed while reading the attention snapshot.',
      ),
    )
  })

  it('rejects an attention snapshot before buffering beyond the byte ceiling', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(' '.repeat(MAX_ATTENTION_SNAPSHOT_BYTES + 1))),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention snapshot exceeds the contract byte ceiling',
    )
  })

  it('rejects an attention event beyond the advertised NDJSON item ceiling', async () => {
    const body = `${' '.repeat(bootstrapFixture.limits.max_ndjson_item_bytes + 1)}\n`
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(body, { headers: { 'content-type': 'application/x-ndjson' } }),
      ),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).rejects.toThrow(
      'attention stream item exceeds the contract ceiling',
    )
  })

  it('rejects a final attention event without its record delimiter', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify(attentionUpdateFixture), {
            headers: { 'content-type': 'application/x-ndjson' },
          }),
      ),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).rejects.toThrow('attention stream ended with an incomplete item')
  })

  it('cancels a follower stream when its consumer reconnects early', async () => {
    let cancelled = false
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(
          new TextEncoder().encode(
            `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n`,
          ),
        )
      },
      cancel() {
        cancelled = true
      },
    })
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(body, { headers: { 'content-type': 'application/x-ndjson' } }),
      ),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).resolves.toEqual({
      done: false,
      value: { kind: 'snapshot', snapshot: attentionFixture },
    })
    await events.return?.(undefined)

    expect(cancelled).toBe(true)
  })
})
