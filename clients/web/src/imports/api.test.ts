import { afterEach, describe, expect, it, vi } from 'vitest'
import { HttpImportApi, ImportListCorrelationError } from './api'

const firstId = '00000000-0000-7000-8000-000000000001'
const secondId = '00000000-0000-7000-8000-000000000002'

const summary = (id: string) => ({
  imported_conversation_id: id,
  display_title: null,
  format: 'claude_code_session_jsonl_v2' as const,
  source_session_id: null,
  entry_count: 1,
})

afterEach(() => vi.unstubAllGlobals())

describe('HttpImportApi correlation', () => {
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

    const window = await new HttpImportApi().entries(firstId, { before: 0, after: 0 })

    expect(window.anchor_position).toBe(1)
  })

  it('rejects a catalog page outside the requested cursor and format', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify({ items: [summary(firstId)], next_cursor: firstId })),
      ),
    )

    await expect(
      new HttpImportApi().list({
        after: secondId,
        format: 'codex_rollout_jsonl_v1',
      }),
    ).rejects.toBeInstanceOf(ImportListCorrelationError)
  })

  it('carries an exact source session as a raw bounded body', async () => {
    const exact = ' source session '
    const fetch = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            items: [
              {
                ...summary(firstId),
                source_session_id: { leading_text: exact, completeness: 'complete' },
              },
            ],
            next_cursor: null,
          }),
        ),
    )
    vi.stubGlobal('fetch', fetch)

    await new HttpImportApi().list({ source_session_id: exact, limit: 1 })

    expect(fetch).toHaveBeenCalledWith(
      '/api/imports/searches?limit=1',
      expect.objectContaining({ body: exact }),
    )
  })
})
