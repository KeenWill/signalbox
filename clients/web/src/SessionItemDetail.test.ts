import { describe, expect, it } from 'vitest'
import type { WebSessionTimelineDetailPage } from './generated/web-contract.mjs'
import { detailBodyMatchesKind } from './SessionItemDetail'

type Detail = WebSessionTimelineDetailPage['items'][number]

const detail = (kind: Detail['kind'], body: Detail['body']): Detail => ({
  address: { event_sequence: '41' },
  kind,
  body,
  projected_body_bytes: 1,
})

describe('timeline detail body correlation', () => {
  it('accepts only body variants correlated with the outer event kind', () => {
    const eventFact = { type: 'event_fact', kind: 'session_created' } as const
    const lifecycle = {
      type: 'turn_lifecycle',
      turn_id: 'turn-1',
      lifecycle: 'activated',
      cause_code: 'activated',
    } as const

    expect(detailBodyMatchesKind(detail('session_created', eventFact))).toBe(true)
    expect(detailBodyMatchesKind(detail('input_accepted', eventFact))).toBe(false)
    expect(
      detailBodyMatchesKind(
        detail('input_accepted', { type: 'event_fact', kind: 'input_accepted' }),
      ),
    ).toBe(false)
    expect(
      detailBodyMatchesKind(
        detail('model_call_transition', {
          type: 'event_fact',
          kind: 'model_call_transition',
        }),
      ),
    ).toBe(false)
    expect(
      detailBodyMatchesKind(
        detail('turn_completed', { type: 'event_fact', kind: 'turn_completed' }),
      ),
    ).toBe(false)
    expect(detailBodyMatchesKind(detail('turn_activated', lifecycle))).toBe(true)
    expect(detailBodyMatchesKind(detail('turn_completed', lifecycle))).toBe(false)
    expect(
      detailBodyMatchesKind(
        detail('turn_reconciliation_required', {
          ...lifecycle,
          lifecycle: 'terminalized',
          cause_code: 'reconciliation_required',
        }),
      ),
    ).toBe(true)
  })
})
