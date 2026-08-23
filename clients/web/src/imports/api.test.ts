import { afterEach, describe, expect, it, vi } from 'vitest'
import { HttpImportApi, ImportListCorrelationError, ImportWindowCorrelationError } from './api'

const firstId = '00000000-0000-7000-8000-000000000001'
const secondId = '00000000-0000-7000-8000-000000000002'
const searchCorrelation = '00000000-0000-7000-8000-000000000003'
const exactSourceSessionDigest = '74bfe0749655154b0b89f676aa1d2f6c9498857529bac49b09715f969d3b4bfc'

const summary = (id: string) => ({
  imported_conversation_id: id,
  display_title: null,
  format: 'claude_code_session_jsonl_v2' as const,
  source_session_id: null,
  source_session_id_sha256: null,
  entry_count: 1,
})

afterEach(() => vi.unstubAllGlobals())

describe('HttpImportApi correlation', () => {
  it('does not issue production import I/O when bootstrap validation fails', async () => {
    const fetch = vi.fn()
    vi.stubGlobal('fetch', fetch)
    const incompatibleBootstrap = new TypeError('bootstrap carries an incompatible web contract')

    await expect(
      new HttpImportApi(() => Promise.reject(incompatibleBootstrap)).list({ limit: 1 }),
    ).rejects.toBe(incompatibleBootstrap)

    expect(fetch).not.toHaveBeenCalled()
  })

  it('revalidates the bootstrap after a transient validation failure', async () => {
    const fetch = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            items: [],
            next_cursor: null,
            search_correlation: null,
            exact_source_session_id_sha256: null,
          }),
        ),
    )
    vi.stubGlobal('fetch', fetch)
    const transientFailure = new TypeError('daemon is restarting')
    const bootstrapValidation = vi
      .fn<() => Promise<void>>()
      .mockRejectedValueOnce(transientFailure)
      .mockResolvedValueOnce(undefined)
    const api = new HttpImportApi(bootstrapValidation)

    await expect(api.list({ limit: 1 })).rejects.toBe(transientFailure)
    await expect(api.list({ limit: 1 })).resolves.toMatchObject({ items: [] })

    expect(bootstrapValidation).toHaveBeenCalledTimes(2)
    expect(fetch).toHaveBeenCalledTimes(1)
  })

  it('accepts the documented omitted first-window anchor', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              anchor_position: 1,
              first_position: 1,
              last_position: 1,
              has_before: false,
              has_after: false,
              items: [
                {
                  frontier: {
                    imported_conversation_id: firstId,
                    imported_entry_id: secondId,
                    position: 1,
                  },
                  raw_record_position: 1,
                  record_entry_position: 1,
                  source_speaker: 'not_attested',
                  content_kind: 'message_content_absent',
                  text: null,
                },
              ],
            }),
          ),
      ),
    )

    const window = await new HttpImportApi(() => Promise.resolve()).entries(firstId, {
      before: 0,
      after: 0,
    })

    expect(window.anchor_position).toBe(1)
  })

  it('rejects an entry window outside the requested radius', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              anchor_position: 1,
              first_position: 1,
              last_position: 2,
              has_before: false,
              has_after: true,
              items: [
                {
                  frontier: {
                    imported_conversation_id: firstId,
                    imported_entry_id: firstId,
                    position: 1,
                  },
                  raw_record_position: 1,
                  record_entry_position: 1,
                  source_speaker: 'not_attested',
                  content_kind: 'message_content_absent',
                  text: null,
                },
                {
                  frontier: {
                    imported_conversation_id: firstId,
                    imported_entry_id: secondId,
                    position: 2,
                  },
                  raw_record_position: 1,
                  record_entry_position: 2,
                  source_speaker: 'not_attested',
                  content_kind: 'message_content_absent',
                  text: null,
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new HttpImportApi(() => Promise.resolve()).entries(firstId, {
        anchor: 'first',
        before: 0,
        after: 0,
      }),
    ).rejects.toBeInstanceOf(ImportWindowCorrelationError)
  })

  it('rejects a latest entry window before the known timeline bound', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              anchor_position: 1,
              first_position: 1,
              last_position: 1,
              has_before: false,
              has_after: true,
              items: [
                {
                  frontier: {
                    imported_conversation_id: firstId,
                    imported_entry_id: secondId,
                    position: 1,
                  },
                  raw_record_position: 1,
                  record_entry_position: 1,
                  source_speaker: 'not_attested',
                  content_kind: 'message_content_absent',
                  text: null,
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new HttpImportApi(() => Promise.resolve()).entries(
        firstId,
        { anchor: 'latest', before: 0, after: 0 },
        undefined,
        1_000,
      ),
    ).rejects.toBeInstanceOf(ImportWindowCorrelationError)
  })

  it('rejects text evidence attached to a non-text content kind', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              anchor_position: 1,
              first_position: 1,
              last_position: 1,
              has_before: false,
              has_after: false,
              items: [
                {
                  frontier: {
                    imported_conversation_id: firstId,
                    imported_entry_id: secondId,
                    position: 1,
                  },
                  raw_record_position: 1,
                  record_entry_position: 1,
                  source_speaker: 'not_attested',
                  content_kind: 'tool_result',
                  text: { kind: 'not_attested' },
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new HttpImportApi(() => Promise.resolve()).entries(firstId, {
        anchor: 'first',
        before: 0,
        after: 0,
      }),
    ).rejects.toBeInstanceOf(ImportWindowCorrelationError)
  })

  it('rejects a catalog page outside the requested cursor and format', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              items: [summary(firstId)],
              next_cursor: firstId,
              search_correlation: null,
              exact_source_session_id_sha256: null,
            }),
          ),
      ),
    )

    await expect(
      new HttpImportApi(() => Promise.resolve()).list({
        after: secondId,
        format: 'codex_rollout_jsonl_v1',
      }),
    ).rejects.toBeInstanceOf(ImportListCorrelationError)
  })

  it('carries an exact source session as a raw bounded body', async () => {
    const exact = ' source session '
    vi.stubGlobal('crypto', { randomUUID: () => searchCorrelation })
    const fetch = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            items: [
              {
                ...summary(firstId),
                source_session_id: { leading_text: exact, completeness: 'complete' },
                source_session_id_sha256: exactSourceSessionDigest,
              },
            ],
            next_cursor: null,
            search_correlation: searchCorrelation,
            exact_source_session_id_sha256: exactSourceSessionDigest,
          }),
        ),
    )
    vi.stubGlobal('fetch', fetch)

    await new HttpImportApi(() => Promise.resolve()).list({ source_session_id: exact, limit: 1 })

    expect(fetch).toHaveBeenCalledWith(
      `/api/imports/searches?limit=1&search_correlation=${searchCorrelation}`,
      expect.objectContaining({ body: exact }),
    )
  })

  it('rejects truncated evidence whose full-value digest does not match the search', async () => {
    vi.stubGlobal('crypto', { randomUUID: () => searchCorrelation })
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              items: [
                {
                  ...summary(firstId),
                  source_session_id: { leading_text: 'shared prefix', completeness: 'truncated' },
                  source_session_id_sha256: '0'.repeat(64),
                },
              ],
              next_cursor: null,
              search_correlation: searchCorrelation,
              exact_source_session_id_sha256: exactSourceSessionDigest,
            }),
          ),
      ),
    )

    await expect(
      new HttpImportApi(() => Promise.resolve()).list({
        source_session_id: 'shared prefix and complete value',
      }),
    ).rejects.toBeInstanceOf(ImportListCorrelationError)
  })
})
