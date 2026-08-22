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
      'attention snapshot exceeds the contract item ceiling',
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
      vi.fn(async () => new Response(body)),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).rejects.toThrow(
      'attention snapshot contains duplicate session identities',
    )
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
      vi.fn(async () => new Response(body)),
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
      vi.fn(async () => new Response(body)),
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
