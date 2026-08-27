import { afterEach, describe, expect, it, vi } from 'vitest'
import { MAX_SESSION_PAGE_ITEMS, SameOriginProductTransport } from './product'

const sessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d'
const previousSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6c'
const summary = {
  action: null,
  active_turn_count: '0',
  archived: false,
  current_turn_id: null,
  goal_block: null,
  judge: { actionable: '0', completed: '3', escalated: '0', failed: '0' },
  last_activity: { kind: 'session' as const, unix_microseconds: '1724200000000000' },
  queued_turn_count: '0',
  session_id: sessionId,
  state: 'idle' as const,
  title_summary: 'Release verification',
  title_truncated: false,
}
const singlePage = {
  continuation: null,
  cursor: '17',
  sort: 'last_activity_descending' as const,
  summaries: [summary],
  total: '1',
}
const fullPage = () => {
  const summaries = Array.from({ length: MAX_SESSION_PAGE_ITEMS }, (_, index) => ({
    ...summary,
    last_activity: {
      kind: 'session' as const,
      unix_microseconds: String(1_724_200_000_000_000 - index),
    },
    session_id: `018f1840-6f3d-7a8b-9c1d-${(0x0e2f3a4b5c6dn + BigInt(index))
      .toString(16)
      .padStart(12, '0')}`,
  }))
  const boundary = summaries.at(-1)
  if (!boundary) throw new Error('full catalog fixture has no continuation boundary')
  return {
    ...singlePage,
    continuation: {
      kind: 'last_activity' as const,
      session_id: boundary.session_id,
      unix_microseconds: boundary.last_activity.unix_microseconds,
    },
    summaries,
    total: '48',
  }
}

afterEach(() => vi.unstubAllGlobals())

describe('session catalog transport', () => {
  it('decodes a bounded page and preserves the landed keyset request', async () => {
    const page = fullPage()
    const fetchMock = vi.fn(async () => new Response(JSON.stringify(page)))
    vi.stubGlobal('fetch', fetchMock)

    await expect(
      new SameOriginProductTransport().readSessions({
        search: 'Release',
        sort: 'activity',
        includeArchived: true,
        afterSession: previousSessionId,
        afterActivity: '1724200000000000',
      }),
    ).resolves.toEqual(page)
    expect(fetchMock).toHaveBeenCalledWith(
      `/api/sessions?include_archived=true&sort=last_activity_descending&search=Release&after_session_id=${previousSessionId}&after_activity_unix_microseconds=1724200000000000`,
      expect.objectContaining({ credentials: 'same-origin' }),
    )
  })

  it('rejects an over-bound search before issuing a request', async () => {
    const fetchMock = vi.fn()
    vi.stubGlobal('fetch', fetchMock)

    await expect(
      new SameOriginProductTransport().readSessions({
        search: 'é'.repeat(513),
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('search exceeds its contract bound')
    expect(fetchMock).not.toHaveBeenCalled()
  })

  it('rejects a page that repeats its exclusive identity cursor', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...singlePage,
              sort: 'session_identity_ascending',
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

  it('rejects a non-truncated row that contradicts the active search', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(singlePage))),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        search: 'different exact text',
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('contradicts the active search')
  })

  it('rejects a continuation on a partial page', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...singlePage,
              continuation: {
                kind: 'last_activity',
                session_id: summary.session_id,
                unix_microseconds: summary.last_activity.unix_microseconds,
              },
              total: '2',
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readSessions({
        sort: 'activity',
        includeArchived: false,
      }),
    ).rejects.toThrow('continued session catalog snapshot is not a full page')
  })
})
