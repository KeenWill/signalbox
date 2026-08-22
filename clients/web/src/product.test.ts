import { afterEach, describe, expect, it, vi } from 'vitest'
import bootstrapFixture from './generated/web-contract-bootstrap.json' with { type: 'json' }
import {
  MAX_PRODUCT_HTTP_RESPONSE_BYTES,
  MAX_SESSION_PAGE_ITEMS,
  MAX_SESSION_SEARCH_BYTES,
  ProductRequestError,
  readProductSessionState,
  SameOriginProductTransport,
} from './product'

const sessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d'
const sessionPageFixture = {
  continuation: {
    kind: 'last_activity',
    session_id: sessionId,
    unix_microseconds: '1724200000000000',
  },
  cursor: '17',
  sort: 'last_activity_descending',
  summaries: [
    {
      action: null,
      active_turn_count: '1',
      archived: false,
      current_turn_id: null,
      goal_block: null,
      judge: { actionable: '0', completed: '3', escalated: '0', failed: '0' },
      last_activity: { kind: 'turn', unix_milliseconds: '1724200000000' },
      queued_turn_count: '2',
      session_id: sessionId,
      state: 'active',
      title_summary: 'Release verification',
      title_truncated: false,
    },
  ],
  total: '48',
} as const
const sessionRequestPath = `/api/sessions?sort=last_activity_desc&include_archived=true&search=release&after_session_id=${sessionId}&after_activity_unix_microseconds=1724200000000000`
const errorFixture = {
  error: {
    code: 'session_catalog_unavailable',
    kind: 'application',
    message: 'session catalog projection is not configured',
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

  it('fails closed when bounded JSON is unavailable', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...bootstrapFixture,
              capabilities: { ...bootstrapFixture.capabilities, bounded_json: false },
            }),
          ),
      ),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      'does not provide bounded JSON',
    )
  })

  it('fails closed when the JSON response ceiling contradicts the browser contract', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...bootstrapFixture,
              limits: { ...bootstrapFixture.limits, max_json_body_bytes: 32_768 },
            }),
          ),
      ),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      'JSON response ceiling contradicts',
    )
  })

  it('reports an unsuccessful HTTP response without decoding its body', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(null, { status: 503 })),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow('status 503')
  })

  it('decodes one bounded session page and preserves its typed cursor request', async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify(sessionPageFixture)))
    vi.stubGlobal('fetch', fetchMock)

    const page = await new SameOriginProductTransport().readSessions({
      search: 'release',
      sort: 'activity',
      includeArchived: true,
      afterSession: sessionId,
      afterActivity: '1724200000000000',
    })

    expect(page).toEqual(sessionPageFixture)
    expect(fetchMock).toHaveBeenCalledWith(
      sessionRequestPath,
      expect.objectContaining({ credentials: 'same-origin' }),
    )
  })

  it('preserves a typed session catalog failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(errorFixture), { status: 503 })),
    )

    const request = new SameOriginProductTransport().readSessions({
      sort: 'identity',
      includeArchived: false,
    })

    await expect(request).rejects.toEqual(
      new ProductRequestError(
        errorFixture.error.code,
        errorFixture.error.kind,
        errorFixture.error.message,
      ),
    )
  })

  it('rejects a response whose sort contradicts the request', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              sort: 'session_identity_ascending',
              continuation: { kind: 'session_identity', session_id: sessionId },
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('contradicts last_activity_descending')
  })

  it('rejects a response whose continuation contradicts the request', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              continuation: { kind: 'session_identity', session_id: sessionId },
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('continuation session_identity contradicts last_activity')
  })

  it('rejects rows that violate the declared activity ordering', async () => {
    const laterSummary = {
      ...sessionPageFixture.summaries[0],
      session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c7e',
      last_activity: { kind: 'turn', unix_milliseconds: '1724200000001' },
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              continuation: null,
              summaries: [sessionPageFixture.summaries[0], laterSummary],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('rows contradict last_activity_descending')
  })

  it('rejects rows that violate the declared identity ordering', async () => {
    const earlierSession = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c5c'
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              sort: 'session_identity_ascending',
              continuation: null,
              summaries: [
                sessionPageFixture.summaries[0],
                { ...sessionPageFixture.summaries[0], session_id: earlierSession },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'identity',
        includeArchived: false,
      }),
    ).rejects.toThrow('rows contradict session_identity_ascending')
  })

  it('rejects identity pages that precede the exclusive request cursor', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              sort: 'session_identity_ascending',
              continuation: null,
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'identity',
        includeArchived: false,
        afterSession: sessionId,
      }),
    ).rejects.toThrow('precedes its identity continuation')
  })

  it('rejects archived rows when the request excludes them', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              summaries: [{ ...sessionPageFixture.summaries[0], archived: true }],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('excluded archived session')
  })

  it('rejects malformed or contradictory catalog totals', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(JSON.stringify({ ...sessionPageFixture, total: 'not-a-number' })),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('contradictory total')
  })

  it('rejects contradictory state and action pairs', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              summaries: [
                { ...sessionPageFixture.summaries[0], state: 'idle', action: 'restore_runner' },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('contradictory state and action')
  })

  it('rejects summaries beyond the daemon scalar ceilings', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              summaries: [{ ...sessionPageFixture.summaries[0], title_summary: '🦀'.repeat(129) }],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('summary scalar ceiling')
  })

  it('rejects a response beyond the catalog page ceiling', async () => {
    const oversizedSummaries = Array.from(
      { length: MAX_SESSION_PAGE_ITEMS + 1 },
      () => sessionPageFixture.summaries[0],
    )
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify({ ...sessionPageFixture, summaries: oversizedSummaries })),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow(`exceeds ${MAX_SESSION_PAGE_ITEMS} summaries`)
  })

  it('rejects activity timestamps outside the JavaScript Date range', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              continuation: null,
              summaries: [
                {
                  ...sessionPageFixture.summaries[0],
                  last_activity: { kind: 'turn', unix_milliseconds: '9007199254740991' },
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('outside the JavaScript Date range')
  })

  it('rejects malformed session identities before exposing a page', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              continuation: { ...sessionPageFixture.continuation, session_id: 'not-a-session' },
              summaries: [{ ...sessionPageFixture.summaries[0], session_id: 'not-a-session' }],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('non-canonical session identity')
  })

  it('rejects a continuation that does not match the returned page boundary', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              continuation: {
                ...sessionPageFixture.continuation,
                session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c7e',
              },
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('does not match its returned boundary')
  })

  it('accepts exact continuation precision within the displayed millisecond', async () => {
    const precisePage = {
      ...sessionPageFixture,
      continuation: { ...sessionPageFixture.continuation, unix_microseconds: '1724200000000999' },
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(precisePage))),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).resolves.toEqual(precisePage)
  })

  it('rejects an invalid search before fetching', async () => {
    const fetchMock = vi.fn()
    vi.stubGlobal('fetch', fetchMock)

    await expect(
      new SameOriginProductTransport().readSessions({
        search: 'x'.repeat(MAX_SESSION_SEARCH_BYTES + 1),
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('search exceeds its contract bound')
    expect(fetchMock).not.toHaveBeenCalled()
  })

  it('rejects a catalog response beyond its encoded byte ceiling before decoding', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(' '.repeat(MAX_PRODUCT_HTTP_RESPONSE_BYTES + 1))),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('exceeds its encoded byte ceiling')
  })
})

describe('readProductSessionState', () => {
  it('keeps only admitted URL-owned catalog fields', () => {
    expect(
      readProductSessionState({
        q: 'release',
        sort: 'identity',
        archived: true,
        afterSession: sessionId,
        session: '',
      }),
    ).toEqual({
      q: 'release',
      sort: 'identity',
      archived: true,
      afterSession: sessionId,
      afterActivity: undefined,
      session: undefined,
    })
  })

  it('drops searches that violate the catalog contract', () => {
    expect(
      readProductSessionState({ q: `release${String.fromCharCode(0)}candidate` }).q,
    ).toBeUndefined()
    expect(readProductSessionState({ q: 'é'.repeat(MAX_SESSION_SEARCH_BYTES) }).q).toBeUndefined()
  })

  it('drops malformed or sort-incompatible URL continuations', () => {
    expect(
      readProductSessionState({ sort: 'identity', afterSession: sessionId, afterActivity: '7' }),
    ).toMatchObject({ afterSession: undefined, afterActivity: undefined })
    expect(readProductSessionState({ afterSession: sessionId })).toMatchObject({
      afterSession: undefined,
      afterActivity: undefined,
    })
    expect(
      readProductSessionState({ afterSession: 'not-a-session', afterActivity: '7' }),
    ).toMatchObject({ afterSession: undefined, afterActivity: undefined })
  })
})
