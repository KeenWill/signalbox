import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  ProductRequestError,
  ProductTransportError,
  readProductSearchState,
  SameOriginProductTransport,
} from './product'

const bootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '1' },
  capabilities: {
    bounded_json: true,
    bounded_lexical_search: true,
    bounded_session_timeline: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
  },
  limits: {
    max_json_body_bytes: 65_536,
    max_ndjson_item_bytes: 262_144,
    max_search_query_bytes: 512,
    max_search_page_items: 100,
    max_search_snippet_bytes: 512,
    max_timeline_window_bytes: 524_288,
    max_timeline_window_items: 256,
  },
} as const

const searchPageFixture = {
  results: [
    {
      session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d',
      address: { event_sequence: '901' },
      source: { kind: 'session', session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d' },
      content_class: 'session_metadata',
      snippet: 'search fixture',
      highlights: [{ start_byte: 0, end_byte: 6 }],
    },
  ],
  continuation: null,
} as const

const errorFixture = {
  error: {
    code: 'search_projection_unavailable',
    kind: 'application',
    message: 'search projection is not configured',
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

  it('rejects a bootstrap response beyond the fixed JSON byte bound', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify(bootstrapFixture), {
            headers: { 'content-length': '65537' },
          }),
      ),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      'response exceeds 65536 bytes',
    )
  })

  it('reports an unsuccessful HTTP response without decoding its body', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(null, { status: 503 })),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow('status 503')
  })

  it('decodes a bounded search page and sends product vocabulary', async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify(searchPageFixture)))
    vi.stubGlobal('fetch', fetchMock)

    const page = await new SameOriginProductTransport().search({
      query: 'natural terms',
      sessionId: searchPageFixture.results[0].session_id,
      maxItems: 100,
      maxSnippetBytes: 512,
      after: { address: '500', projectionId: '42' },
    })

    expect(page).toEqual(searchPageFixture)
    expect(fetchMock).toHaveBeenCalledWith(
      `/api/search?strategy=lexical&q=natural+terms&max_items=100&session_id=${searchPageFixture.results[0].session_id}&after_address=500&after_projection=42`,
      expect.objectContaining({ credentials: 'same-origin' }),
    )
  })

  it('preserves a typed application search failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(errorFixture), { status: 503 })),
    )

    const request = new SameOriginProductTransport().search({
      query: 'term',
      maxItems: 10,
      maxSnippetBytes: 512,
    })

    await expect(request).rejects.toEqual(
      new ProductRequestError(
        errorFixture.error.code,
        errorFixture.error.kind,
        errorFixture.error.message,
      ),
    )
  })

  it('distinguishes an unreachable search transport from contract decoding failures', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => Promise.reject(new TypeError('Failed to fetch'))),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 10,
        maxSnippetBytes: 512,
      }),
    ).rejects.toEqual(new ProductTransportError('The search request could not reach Signalbox.'))
  })

  it('rejects an encoded search response beyond its byte bound', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify(searchPageFixture), {
            headers: { 'content-length': '9999999' },
          }),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('response exceeds')
  })

  it('rejects decoded search fields beyond their rendering bounds', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [{ ...searchPageFixture.results[0], snippet: 'too long' }],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 3,
      }),
    ).rejects.toThrow('snippet limit')
  })
  it('rejects results outside an exact-session request', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [{ ...searchPageFixture.results[0], session_id: 'wrong-session' }],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        sessionId: searchPageFixture.results[0].session_id,
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('outside the requested session')
  })

  it('rejects highlight offsets inside a UTF-8 character', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [
                {
                  ...searchPageFixture.results[0],
                  snippet: 'évidence',
                  highlights: [{ start_byte: 1, end_byte: 2 }],
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('invalid highlight range')
  })

  it('rejects search pages that are not ordered newest first', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [
                { ...searchPageFixture.results[0], address: { event_sequence: '750' } },
                { ...searchPageFixture.results[0], address: { event_sequence: '901' } },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 2,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('not ordered newest first')
  })
})

describe('readProductSearchState', () => {
  it('keeps only nonempty typed URL fields', () => {
    expect(
      readProductSearchState({ q: 'term', session: '', afterAddress: 7, around: '901' }),
    ).toEqual({
      q: 'term',
      session: undefined,
      afterAddress: undefined,
      afterProjection: undefined,
      around: '901',
    })
  })

  it('preserves JSON-like numeric and boolean query text', () => {
    expect(readProductSearchState({ q: 2026 }).q).toBe('2026')
    expect(readProductSearchState({ q: true }).q).toBe('true')
    expect(readProductSearchState({ q: null }).q).toBe('null')
  })
})
