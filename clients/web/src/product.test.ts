import { afterEach, describe, expect, it, vi } from 'vitest'
import bootstrapFixture from './generated/web-contract-bootstrap.json' with { type: 'json' }
import {
  BootstrapContractError,
  MAX_BOOTSTRAP_RESPONSE_BYTES,
  MAX_PRODUCT_HTTP_RESPONSE_BYTES,
  MAX_SESSION_PAGE_ITEMS,
  MAX_SESSION_SEARCH_BYTES,
  ProductRequestError,
  readProductSessionState,
  SameOriginProductTransport,
} from './product'
import { webContractBootstrapFixture } from './product.fixture'

const sessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d'
const previousSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6c'
const currentTurnId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5d80'
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
      current_turn_id: currentTurnId,
      goal_block: null,
      judge: { actionable: '0', completed: '3', escalated: '0', failed: '0' },
      last_activity: { kind: 'turn', unix_microseconds: '1724200000000000' },
      queued_turn_count: '2',
      session_id: sessionId,
      state: 'active',
      title_summary: 'Release verification',
      title_truncated: false,
    },
  ],
  total: '48',
} as const

const fullActivityPageFixture = () => {
  const summaries = Array.from({ length: MAX_SESSION_PAGE_ITEMS }, (_, index) => ({
    ...sessionPageFixture.summaries[0],
    session_id: `018f1840-6f3d-7a8b-9c1d-${(0x0e2f3a4b5c6dn + BigInt(index))
      .toString(16)
      .padStart(12, '0')}`,
    last_activity: {
      kind: 'turn' as const,
      unix_microseconds: String(1_724_200_000_000_000 - index),
    },
    title_summary: `release session ${index + 1}`,
  }))
  const boundary = summaries.at(-1)
  if (!boundary) throw new Error('full activity fixture has a boundary')
  return {
    ...sessionPageFixture,
    summaries,
    continuation: {
      kind: 'last_activity' as const,
      session_id: boundary.session_id,
      unix_microseconds: boundary.last_activity.unix_microseconds,
    },
  }
}

const activityPageBoundary = (page: ReturnType<typeof fullActivityPageFixture>) => {
  const boundary = page.summaries.at(-1)
  if (!boundary) throw new Error('full activity fixture has a boundary')
  return boundary
}

const sessionRequestPath = `/api/sessions?sort=last_activity_desc&include_archived=true&search=release&after_session_id=${previousSessionId}&after_activity_unix_microseconds=1724200000000000`
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

  it('fails closed when the bootstrap identity contradicts the generated contract', async () => {
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
      'violates the web contract',
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
    const pageFixture = fullActivityPageFixture()
    const fetchMock = vi.fn(async () => new Response(JSON.stringify(pageFixture)))
    vi.stubGlobal('fetch', fetchMock)

    const page = await new SameOriginProductTransport().readSessions({
      search: 'release',
      sort: 'activity',
      includeArchived: true,
      afterSession: previousSessionId,
      afterActivity: '1724200000000000',
    })

    expect(page).toEqual(pageFixture)
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

  it('preserves a session catalog transport failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => Promise.reject(new TypeError('Failed to fetch'))),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'identity',
        includeArchived: false,
      }),
    ).rejects.toEqual(
      new ProductRequestError(
        'session_catalog_transport_unavailable',
        'transport',
        'The session catalog transport is unavailable.',
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
    ).rejects.toThrow('the continuation required by sort last_activity_descending')
  })

  it('rejects rows that violate the declared activity ordering', async () => {
    const laterSummary = {
      ...sessionPageFixture.summaries[0],
      session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c7e',
      last_activity: { kind: 'turn', unix_microseconds: '1724200000001000' },
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
    ).rejects.toThrow('strictly ordered by sort last_activity_descending')
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
    ).rejects.toThrow('strictly ordered by sort session_identity_ascending')
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

  it('rejects activity pages that precede the exclusive request cursor', async () => {
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
                  last_activity: { kind: 'turn', unix_microseconds: '1724200000001000' },
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
        afterSession: sessionId,
        afterActivity: '1724200000000999',
      }),
    ).rejects.toThrow('precedes its activity continuation')
  })

  it('rejects activity pages that repeat the exact request boundary identity', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              continuation: null,
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
        afterSession: sessionId,
        afterActivity: '1724200000000000',
      }),
    ).rejects.toThrow('repeats its exact activity continuation boundary')
  })

  it('rejects rows that contradict an exact active search', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              continuation: null,
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        search: 'missing exact text',
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('contradicts the active search')
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
    ).rejects.toThrow('attention_snapshot.total must be')
  })

  it('rejects malformed catalog cursors', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              continuation: null,
              cursor: 'not-a-number',
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('attention_snapshot.cursor must be')
  })

  it('rejects a zero cursor on a nonempty catalog page', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify({ ...sessionPageFixture, continuation: null, cursor: '0' })),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('contradictory numeric page field')
  })

  it('requires a continuation when an initial page omits matching rows', async () => {
    const pageFixture = fullActivityPageFixture()
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify({ ...pageFixture, continuation: null, total: '48' })),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('omits a required continuation')
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
    ).rejects.toThrow('the action required by state idle')
  })

  it('rejects blocked rows without blocked-goal evidence', async () => {
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
                  state: 'blocked',
                  action: 'provide_goal_need',
                  goal_block: null,
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
    ).rejects.toThrow('present exactly for blocked state')
  })

  it('rejects goal-block evidence on unrelated states', async () => {
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
                  goal_block: {
                    generation: '1',
                    reason: 'user_input_required',
                    need_summary: 'Choose a target.',
                  },
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
    ).rejects.toThrow('present exactly for blocked state')
  })

  it('rejects impossible title truncation flags', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              continuation: null,
              summaries: [{ ...sessionPageFixture.summaries[0], title_truncated: true }],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('exactly 128 Unicode scalar values when title_truncated is true')
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
    ).rejects.toThrow('at most 128 Unicode scalar values')
  })

  it('rejects blocked-goal summaries beyond the daemon scalar ceiling', async () => {
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
                  state: 'blocked',
                  action: 'provide_goal_need',
                  goal_block: {
                    generation: '1',
                    reason: 'user_input_required',
                    need_summary: '🦀'.repeat(129),
                  },
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
    ).rejects.toThrow('goal_block must be one recognized variant')
  })

  it('rejects displayed turn counts that are not canonical u64 values', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              continuation: null,
              summaries: [{ ...sessionPageFixture.summaries[0], active_turn_count: '01' }],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('active_turn_count must be')
  })

  it('rejects non-canonical approval-judge counts', async () => {
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
                  judge: { ...sessionPageFixture.summaries[0].judge, actionable: 'NaN' },
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
    ).rejects.toThrow('judge.actionable must be')
  })

  it('rejects zero goal generations', async () => {
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
                  state: 'blocked',
                  action: 'provide_goal_need',
                  goal_block: {
                    generation: '0',
                    reason: 'user_input_required',
                    need_summary: 'Choose a target.',
                  },
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
    ).rejects.toThrow('goal_block must be one recognized variant')
  })

  it('rejects pre-epoch activity timestamps', async () => {
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
                  last_activity: { kind: 'turn', unix_microseconds: '-1' },
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
    ).rejects.toThrow('last_activity.unix_microseconds must be')
  })

  it('rejects duplicate session identities on activity pages', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              continuation: null,
              summaries: [
                sessionPageFixture.summaries[0],
                {
                  ...sessionPageFixture.summaries[0],
                  last_activity: { kind: 'turn', unix_microseconds: '1724199999999000' },
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
    ).rejects.toThrow('duplicate session identity')
  })

  it('rejects continuations attached to partial catalog pages', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(sessionPageFixture))),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('continuation accompanies a partial page')
  })

  it('rejects continuations that contradict the declared total', async () => {
    const pageFixture = fullActivityPageFixture()
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify({ ...pageFixture, total: '16' }))),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('continuation contradicts the declared total')
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
    ).rejects.toThrow('at most 16 items')
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
                  last_activity: { kind: 'turn', unix_microseconds: '9007199254740991000' },
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
    ).rejects.toThrow('continuation must be one recognized variant')
  })

  it('rejects non-canonical current-turn identities before exposing a page', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              continuation: null,
              summaries: [{ ...sessionPageFixture.summaries[0], current_turn_id: 'turn-31' }],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('current_turn_id must be one recognized variant')
  })

  it('rejects turn-derived states without a current-turn identity', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...sessionPageFixture,
              continuation: null,
              summaries: [{ ...sessionPageFixture.summaries[0], current_turn_id: null }],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('a turn identity for state active')
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
    ).rejects.toThrow('the session of the last returned summary')
  })

  it('rejects a continuation whose activity timestamp skews from its boundary', async () => {
    const pageFixture = fullActivityPageFixture()
    const boundary = activityPageBoundary(pageFixture)
    const skewedPage = {
      ...pageFixture,
      continuation: {
        ...pageFixture.continuation,
        unix_microseconds: String(BigInt(boundary.last_activity.unix_microseconds) + 1n),
      },
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(skewedPage))),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('the activity timestamp of the last returned summary')
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
