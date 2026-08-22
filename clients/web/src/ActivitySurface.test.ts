import { describe, expect, it } from 'vitest'
import { retainActivityPage } from './ActivitySurface'
import type { WebRepoWatchActivityPage } from './generated/web-contract.mjs'

const page = (receiptSequence: string): WebRepoWatchActivityPage => ({
  event_continuation_before: null,
  events: [],
  webhook_continuation_before_receipt_sequence: null,
  webhooks: [
    {
      action_name: 'opened',
      disposition: 'projected',
      event_name: 'pull_request',
      latest_projected_at_unix_milliseconds: '1724200000000',
      projection_count: '1',
      receipt_sequence: receiptSequence,
      received_at_unix_milliseconds: '1724200000000',
    },
  ],
})

describe('retained activity pages', () => {
  it('replaces a refetched cursor page without appending a duplicate', () => {
    const first = retainActivityPage([], 'cursor-1', page('1'))
    const refreshed = retainActivityPage(first, 'cursor-1', page('2'))

    expect(refreshed).toHaveLength(1)
    expect(refreshed[0]?.page.webhooks[0]?.receipt_sequence).toBe('2')
  })
})
