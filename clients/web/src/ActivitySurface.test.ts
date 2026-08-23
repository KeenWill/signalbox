import { describe, expect, it } from 'vitest'
import { readinessLabel, retainActivityPage, singletonScopeLabel } from './ActivitySurface'
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

describe('work singleton labels', () => {
  it('keeps pull-request, stack, rule, and repository scopes distinct', () => {
    expect(
      singletonScopeLabel({
        kind: 'pull_request',
        repository: 'example/repository',
        number: '17',
      }),
    ).toBe('PR example/repository#17')
    expect(
      singletonScopeLabel({
        kind: 'stack',
        repository: 'example/repository',
        root_pull_request: '11',
      }),
    ).toBe('Stack example/repository#11')
    expect(singletonScopeLabel({ kind: 'rule' })).toBe('Rule-wide')
    expect(singletonScopeLabel({ kind: 'repository', repository: 'example/repository' })).toBe(
      'Repository example/repository',
    )
  })
})

describe('queued-work readiness labels', () => {
  it('preserves variant-specific readiness evidence', () => {
    expect(readinessLabel({ kind: 'ready' })).toBe('ready')
    expect(
      readinessLabel({
        dispatch_id: 'dispatch-1',
        kind: 'occupied',
        session_ids: ['session-1', 'session-2'],
      }),
    ).toBe('occupied · dispatch dispatch-1 · sessions session-1, session-2')
    expect(readinessLabel({ eligible_at_unix_milliseconds: null, kind: 'cooldown' })).toBe(
      'cooldown · eligibility not scheduled',
    )
    expect(
      readinessLabel({ kind: 'parked', parked_at_unix_milliseconds: '9007199254740991' }),
    ).toBe('parked · since 9007199254740991')
  })
})
