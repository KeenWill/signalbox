import { describe, expect, it } from 'vitest'
import { decodeWebSessionTimelineDetailPage } from './generated/web-contract.mjs'

const ambiguousModelCallItem = {
  address: { event_sequence: '7' },
  kind: 'model_call_transition',
  body: {
    type: 'model_call',
    turn_id: '00000000-0000-0000-0000-000000000002',
    model_call_id: '00000000-0000-0000-0000-000000000003',
    state: { type: 'terminal', disposition: 'ambiguous' },
    model_identity_id: 'anthropic:claude-sonnet',
    request_context_items: '4',
    usage: {},
    cause_code: 'transport_interrupted',
  },
  projected_body_bytes: 128,
}

const ambiguousModelCallPage = {
  session_id: '00000000-0000-0000-0000-000000000001',
  items: [ambiguousModelCallItem],
  projected_body_bytes: 128,
}

describe('generated timeline detail decoder', () => {
  it('accepts an ambiguous terminal model call with its disposition in-band', () => {
    const page = decodeWebSessionTimelineDetailPage(ambiguousModelCallPage)

    expect(page.items[0]?.body).toEqual(ambiguousModelCallPage.items[0]?.body)
  })

  it('rejects a terminal model call without a disposition', () => {
    const invalidPage = {
      ...ambiguousModelCallPage,
      items: [
        {
          ...ambiguousModelCallItem,
          body: { ...ambiguousModelCallItem.body, state: { type: 'terminal' } },
        },
      ],
    }

    expect(() => decodeWebSessionTimelineDetailPage(invalidPage)).toThrow(
      'timeline_detail_page.items[0].body must be one recognized variant',
    )
  })
})
