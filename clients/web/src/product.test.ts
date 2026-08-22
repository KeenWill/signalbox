import { afterEach, describe, expect, it, vi } from 'vitest'
import bootstrapFixture from './generated/web-contract-bootstrap.json' with { type: 'json' }
import {
  MAX_PRODUCT_HTTP_RESPONSE_BYTES,
  MAX_SESSION_PAGE_ITEMS,
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
        afterActivity: 7,
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
})
