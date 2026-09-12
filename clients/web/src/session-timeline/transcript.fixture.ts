import { webContractBootstrapFixture } from '../product.fixture'

export const transcriptSessionId = '00000000-0000-0000-0000-000000000991'
export const transcriptSize = 100_000
export function transcriptFixture(url: URL): unknown {
  if (url.pathname === '/api/bootstrap') return webContractBootstrapFixture
  if (url.pathname.endsWith('/timeline-detail')) {
    const sequence = url.searchParams.get('first') ?? '1'
    const text = `Message ${sequence}`
    return {
      session_id: transcriptSessionId,
      projected_body_bytes: 128 + text.length,
      continuation: null,
      items: [
        {
          address: { event_sequence: sequence },
          kind: 'input_accepted',
          projected_body_bytes: 128 + text.length,
          body: {
            type: 'user_input',
            turn_id: `00000000-0000-0000-0000-${sequence.padStart(12, '0')}`,
            attachments: [],
            text: { text, offset_bytes: '0', total_bytes: String(text.length), continuation: null },
          },
        },
      ],
    }
  }
  if (url.pathname.endsWith('/timeline')) {
    const count = Number(url.searchParams.get('max_items') ?? '80')
    const address = Number(url.searchParams.get('address') ?? '1')
    const anchor = url.searchParams.get('anchor')
    const first = Math.max(
      1,
      anchor === 'latest'
        ? transcriptSize - count + 1
        : anchor === 'before'
          ? address - count
          : anchor === 'after'
            ? address + 1
            : anchor === 'around'
              ? address - Math.floor(count / 2)
              : 1,
    )
    const last = Math.min(
      transcriptSize,
      first + count - 1,
      anchor === 'before' ? address - 1 : transcriptSize,
    )
    const items = Array.from({ length: Math.max(0, last - first + 1) }, (_, index) => ({
      address: { event_sequence: String(first + index) },
      kind: 'input_accepted',
      projected_structured_bytes: 78,
    }))
    return {
      session_id: transcriptSessionId,
      items,
      projected_structured_bytes: items.length * 78,
      continuation_before: first > 1 ? { event_sequence: String(first) } : null,
      continuation_after: last < transcriptSize ? { event_sequence: String(last) } : null,
    }
  }
  if (url.pathname.endsWith('/live'))
    return {
      session_id: transcriptSessionId,
      observed_through: String(transcriptSize),
      active: null,
      queued_turn_count: '0',
      queued_turn_ids: [],
      reconciliation: null,
      runner: null,
    }
  return {
    session_id: transcriptSessionId,
    supervision: null,
    repository_watch: null,
    workspace_root_kind: null,
    sizes: {
      item_count: String(transcriptSize),
      projected_text_bytes: String(transcriptSize * 12),
      projected_structured_bytes: String(transcriptSize * 78),
      referenced_blob_count: '0',
      referenced_blob_bytes: '0',
    },
    first_address: { event_sequence: '1' },
    latest_address: { event_sequence: String(transcriptSize) },
    observed_through: String(transcriptSize),
    work: { active_turn_count: '0', queued_turn_count: '0' },
  }
}
