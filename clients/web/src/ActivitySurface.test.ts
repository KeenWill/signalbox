import { describe, expect, it } from 'vitest'
import {
  automationLabel,
  heldOwnershipLabel,
  readinessLabel,
  retainActivityPage,
  sessionPurposeLabel,
  singletonScopeLabel,
} from './ActivitySurface'
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
    const refreshedFixture = page('2')
    const refreshed = retainActivityPage(first, 'cursor-1', refreshedFixture)

    expect(refreshed).toHaveLength(1)
    expect(refreshed[0]?.page.webhooks[0]?.receipt_sequence).toBe(
      refreshedFixture.webhooks[0]?.receipt_sequence,
    )
  })
})

describe('automation convergence labels', () => {
  it('preserves typed dispatch, event, and settlement evidence', () => {
    expect(automationLabel({ kind: 'held', dispatch_id: 'dispatch-1' })).toBe(
      'held · dispatch dispatch-1',
    )
    expect(
      automationLabel({
        kind: 'stale_seal',
        dispatch_id: 'dispatch-2',
        sealed_event_id: 'event-2',
      }),
    ).toBe('stale seal · dispatch dispatch-2 · event event-2')
    expect(
      automationLabel({
        kind: 'current_head_sealed',
        dispatch_id: 'dispatch-3',
        sealed_event_id: 'event-3',
        settled_at_unix_milliseconds: '9007199254740991',
      }),
    ).toBe('current head sealed · dispatch dispatch-3 · event event-3 · settled 9007199254740991')
  })
})

describe('commissioned-session purpose labels', () => {
  it('preserves rule and operator provenance', () => {
    expect(
      sessionPurposeLabel({
        kind: 'rule_dispatch',
        template: 'review',
        rule: 'converge',
        dispatch_id: 'dispatch-4',
        event_id: 'event-4',
      }),
    ).toBe('rule dispatch · review · rule converge · dispatch dispatch-4 · event event-4')
    expect(
      sessionPurposeLabel({
        kind: 'operator_commission',
        template: 'inspect',
        dispatch_id: 'dispatch-5',
      }),
    ).toBe('operator commission · inspect · dispatch dispatch-5')
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
    expect(
      readinessLabel({
        kind: 'externally_blocked',
        session_ids: ['session-3'],
      }),
    ).toBe('externally blocked · sessions session-3')
    expect(readinessLabel({ eligible_at_unix_milliseconds: null, kind: 'cooldown' })).toBe(
      'cooldown · eligibility not scheduled',
    )
    expect(
      readinessLabel({ kind: 'parked', parked_at_unix_milliseconds: '9007199254740991' }),
    ).toBe('parked · since 9007199254740991')
  })
})

describe('held-work ownership labels', () => {
  it('preserves dispatch and session identities', () => {
    expect(
      heldOwnershipLabel({
        blockers: ['pursuing_goal'],
        dispatch_id: 'dispatch-4',
        held_since_unix_microseconds: '1724200000000000',
        rule: 'review',
        scope: { kind: 'repository', repository: 'example/repository' },
        session_ids: ['session-1', 'session-2'],
      }),
    ).toBe('dispatch dispatch-4 · sessions session-1, session-2')
  })
})
