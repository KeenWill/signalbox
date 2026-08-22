import { afterEach, describe, expect, it, vi } from 'vitest'
import { ProductRequestError, readProductSessionState, SameOriginProductTransport } from './product'

const bootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '1' },
  capabilities: {
    bounded_json: true,
    bounded_session_timeline: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
  },
  limits: {
    max_json_body_bytes: 65_536,
    max_ndjson_item_bytes: 262_144,
    max_timeline_window_bytes: 524_288,
    max_timeline_window_items: 256,
  },
} as const

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
