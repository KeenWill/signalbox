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
      projection_id: '42',
      source: { kind: 'session', session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d' },
      content_class: 'session_metadata',
      snippet: 'search fixture',
      highlights: [{ start_byte: 0, end_byte: 6 }],
    },
  ],
  continuation: null,
} as const

const escapedSearchPageFixture = {
  results: Array.from({ length: 100 }, (_, index) => ({
    ...searchPageFixture.results[0],
    address: { event_sequence: String(100 - index) },
    snippet: '\0'.repeat(512),
    highlights: [],
  })),
  continuation: null,
}

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

  it('rejects a mismatched bootstrap contract identity', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...bootstrapFixture,
              contract: { ...bootstrapFixture.contract, version: '2' },
            }),
          ),
      ),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      'incompatible web contract',
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

  it('distinguishes an unreachable bootstrap transport from contract decoding failures', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => Promise.reject(new TypeError('Failed to fetch'))),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toEqual(
      new ProductTransportError('The bootstrap request could not reach Signalbox.'),
    )
  })

  it('preserves transport identity when a bootstrap response stream is interrupted', async () => {
    const body = new ReadableStream<Uint8Array>({
      pull(controller) {
        controller.error(new TypeError('connection reset'))
      },
    })
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(body)),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toEqual(
      new ProductTransportError('The bootstrap response stream was interrupted.'),
    )
  })

  const rejectBootstrapLimits = async (limits: Record<string, number>) => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...bootstrapFixture,
              limits: { ...bootstrapFixture.limits, ...limits },
            }),
          ),
      ),
    )
    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow('invalid search')
  }

  it('rejects a zero search-query ceiling', () =>
    rejectBootstrapLimits({ max_search_query_bytes: 0 }))

  it('rejects an excessive search-query ceiling', () =>
    rejectBootstrapLimits({ max_search_query_bytes: 513 }))

  it('rejects a zero search-page ceiling', () =>
    rejectBootstrapLimits({ max_search_page_items: 0 }))

  it('rejects an excessive search-page ceiling', () =>
    rejectBootstrapLimits({ max_search_page_items: 101 }))

  it('rejects a zero search-snippet ceiling', () =>
    rejectBootstrapLimits({ max_search_snippet_bytes: 0 }))

  it('rejects an excessive search-snippet ceiling', () =>
    rejectBootstrapLimits({ max_search_snippet_bytes: 513 }))

  it('decodes a bounded search page and sends product vocabulary', async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify(searchPageFixture)))
    vi.stubGlobal('fetch', fetchMock)
    const request = {
      query: 'natural terms',
      sessionId: searchPageFixture.results[0].session_id,
      maxItems: 100,
      maxSnippetBytes: 512,
      after: { address: '1000', projectionId: '42' },
    }
    const expectedSearch = new URLSearchParams({
      strategy: 'lexical',
      q: request.query,
      max_items: String(request.maxItems),
      session_id: request.sessionId,
      after_address: request.after.address,
      after_projection: request.after.projectionId,
    })

    const page = await new SameOriginProductTransport().search(request)

    expect(page).toEqual(searchPageFixture)
    expect(fetchMock).toHaveBeenCalledWith(
      `/api/search?${expectedSearch.toString()}`,
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

  it('preserves transport identity when a search response stream is interrupted', async () => {
    const body = new ReadableStream<Uint8Array>({
      pull(controller) {
        controller.error(new TypeError('connection reset'))
      },
    })
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(body)),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toEqual(new ProductTransportError('The search response stream was interrupted.'))
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

  it('accepts bounded snippets at their worst-case JSON expansion', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(escapedSearchPageFixture))),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 100,
        maxSnippetBytes: 512,
      }),
    ).resolves.toEqual(escapedSearchPageFixture)
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
              results: [
                {
                  ...searchPageFixture.results[0],
                  session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c7e',
                  source: {
                    kind: 'session',
                    session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c7e',
                  },
                },
              ],
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

  it('canonicalizes exact-session UUID spellings before comparing results', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(searchPageFixture))),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        sessionId: `URN:UUID:{${searchPageFixture.results[0].session_id.toUpperCase()}}`,
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).resolves.toEqual(searchPageFixture)
  })

  it('rejects session source identities that contradict the result session', async () => {
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
                  source: {
                    kind: 'session',
                    session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c7e',
                  },
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
    ).rejects.toThrow('source consistent with the result session and content class')
  })

  it('rejects a malformed result session UUID', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [{ ...searchPageFixture.results[0], session_id: 'not-a-uuid' }],
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
    ).rejects.toThrow('session_id must be a string matching')
  })

  it('rejects a malformed typed source UUID', async () => {
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
                  source: { kind: 'session', session_id: 'not-a-uuid' },
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
    ).rejects.toThrow('source must be one recognized variant')
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
    ).rejects.toThrow('UTF-8 boundaries')
  })

  it('rejects more highlights than the generated contract permits', async () => {
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
                  snippet: 'x'.repeat(130),
                  highlights: Array.from({ length: 65 }, (_, index) => ({
                    start_byte: index * 2,
                    end_byte: index * 2 + 1,
                  })),
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
    ).rejects.toThrow('at most 64 items')
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
    ).rejects.toThrow('strictly descending search result key')
  })

  it('rejects same-address results whose projection IDs are not newest first', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [
                { ...searchPageFixture.results[0], projection_id: '41' },
                { ...searchPageFixture.results[0], projection_id: '42' },
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
    ).rejects.toThrow('strictly descending search result key')
  })

  it('rejects a result address above u64', async () => {
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
                  address: { event_sequence: '18446744073709551616' },
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
    ).rejects.toThrow('unsigned 64-bit integer')
  })

  it('rejects a result projection ID above positive i64', async () => {
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
                  projection_id: '9223372036854775808',
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
    ).rejects.toThrow('positive signed 64-bit integer')
  })

  it('rejects a page that repeats the request cursor', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(searchPageFixture))),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
        after: {
          address: searchPageFixture.results[0].address.event_sequence,
          projectionId: searchPageFixture.results[0].projection_id,
        },
      }),
    ).rejects.toThrow('does not advance past the request cursor')
  })

  it('rejects a page newer than the request cursor', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(searchPageFixture))),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
        after: { address: '900', projectionId: '42' },
      }),
    ).rejects.toThrow('does not advance past the request cursor')
  })

  const rejectContinuation = async (
    continuation: {
      address: { event_sequence: string }
      projection_id: string
    },
    expectedError = 'invalid continuation',
  ) => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify({ ...searchPageFixture, continuation }))),
    )
    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow(expectedError)
  }

  it('rejects a continuation address detached from the returned page', () =>
    rejectContinuation(
      { address: { event_sequence: '900' }, projection_id: '42' },
      'last result ordering key',
    ))

  it('rejects a continuation projection detached from the returned page', () =>
    rejectContinuation(
      { address: { event_sequence: '901' }, projection_id: '41' },
      'last result ordering key',
    ))

  it('rejects a nondecimal continuation projection ID', () =>
    rejectContinuation(
      { address: { event_sequence: '901' }, projection_id: 'not-decimal' },
      'recognized variant',
    ))

  it('rejects a zero continuation projection ID', () =>
    rejectContinuation(
      { address: { event_sequence: '901' }, projection_id: '0' },
      'recognized variant',
    ))

  it('rejects a continuation projection ID above positive i64', () =>
    rejectContinuation(
      { address: { event_sequence: '901' }, projection_id: '9223372036854775808' },
      'recognized variant',
    ))

  it('rejects a continuation projection ID above u64', () =>
    rejectContinuation(
      { address: { event_sequence: '901' }, projection_id: '18446744073709551616' },
      'recognized variant',
    ))

  it('rejects a continuation on an empty page', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              results: [],
              continuation: { address: { event_sequence: '901' }, projection_id: '42' },
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
    ).rejects.toThrow('last result ordering key')
  })
})

describe('readProductSearchState', () => {
  it('keeps only nonempty typed URL fields', () => {
    expect(
      readProductSearchState({ q: 'term', session: '', afterAddress: 7, around: '901' }),
    ).toEqual({
      q: 'term',
      session: undefined,
      afterAddress: '7',
      afterProjection: undefined,
      around: '901',
    })
  })

  it('preserves JSON-like numeric and boolean query text', () => {
    expect(readProductSearchState({ q: 2026 }).q).toBe('2026')
    expect(readProductSearchState({ q: true }).q).toBe('true')
    expect(readProductSearchState({ q: null }).q).toBe('null')
  })

  it('preserves numeric-looking cursor fields for pair validation', () => {
    expect(readProductSearchState({ q: 'term', afterAddress: 750 })).toEqual({
      q: 'term',
      session: undefined,
      afterAddress: '750',
      afterProjection: undefined,
      around: undefined,
    })
  })

  it('preserves repeated exact-session parameters as invalid state', () => {
    expect(readProductSearchState({ q: 'term', session: ['first', 'second'] })).toEqual({
      q: 'term',
      session: undefined,
      sessionParameterIsValid: false,
      afterAddress: undefined,
      afterProjection: undefined,
      around: undefined,
    })
  })

  it('preserves repeated cursor parameters as invalid state', () => {
    expect(
      readProductSearchState({
        q: 'term',
        afterAddress: ['750', '700'],
        afterProjection: ['42', '41'],
      }),
    ).toEqual({
      q: 'term',
      session: undefined,
      afterAddress: undefined,
      afterProjection: undefined,
      cursorParametersAreValid: false,
      around: undefined,
    })
  })
})
