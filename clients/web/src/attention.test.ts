import { describe, expect, it } from 'vitest'
import { reduceAttentionEvent, synchronizeAttention } from './attention'
import type { WebAttentionSnapshot, WebAttentionStreamEvent } from './generated/web-contract.mjs'
import type { ProductTransport } from './product'

const sessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d'
const anotherSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6e'
const summary = {
  action: 'decide_approval',
  current_turn_id: 'turn-31',
  goal_block: null,
  judge: { actionable: '2', completed: '7', escalated: '1', failed: '0' },
  last_activity: { kind: 'approval_judge', unix_milliseconds: '1724200000000' },
  session_id: sessionId,
  state: 'awaiting_approval',
} as const
const snapshot = {
  continuation_after_session_id: sessionId,
  cursor: '17',
  summaries: [summary],
} as const satisfies WebAttentionSnapshot
const replacement = {
  ...summary,
  action: null,
  state: 'active',
} as const

const streamTransport = (
  batches: ReadonlyArray<ReadonlyArray<WebAttentionStreamEvent>>,
): ProductTransport => {
  let nextBatch = 0
  return {
    readBootstrap: async () => {
      throw new Error('unused bootstrap read')
    },
    readAttention: async () => {
      throw new Error('unused attention read')
    },
    followAttention: async function* () {
      const batch = batches[nextBatch] ?? []
      nextBatch += 1
      for (const event of batch) yield event
    },
  }
}

describe('attention projection recovery', () => {
  it('replaces only summaries already present in the bounded page', () => {
    const reduction = reduceAttentionEvent(snapshot, {
      kind: 'update',
      cursor: '18',
      summaries: [replacement],
    })

    expect(reduction).toEqual({
      kind: 'projection',
      snapshot: { ...snapshot, cursor: '18', summaries: [replacement] },
    })
  })

  it('requests resynchronization when an update names an unloaded session', () => {
    const reduction = reduceAttentionEvent(snapshot, {
      kind: 'update',
      cursor: '18',
      summaries: [{ ...replacement, session_id: anotherSessionId }],
    })

    expect(reduction).toEqual({ kind: 'resync' })
  })

  it('restarts after an explicit resync and marks a cleanly ended monitor stale', async () => {
    const phases: string[] = []
    const projections: WebAttentionSnapshot[] = []
    const controller = new AbortController()
    const transport = streamTransport([
      [
        { kind: 'snapshot', snapshot },
        { kind: 'resync_required', cursor: '18' },
      ],
      [{ kind: 'snapshot', snapshot: { ...snapshot, cursor: '19' } }],
    ])

    await synchronizeAttention({
      transport,
      signal: controller.signal,
      onPhase: (phase) => phases.push(phase),
      onProjection: (projection) => projections.push(projection),
    })

    expect(phases).toEqual(['connecting', 'live', 'resyncing', 'live', 'stale'])
    expect(projections).toEqual([snapshot, { ...snapshot, cursor: '19' }])
  })

  it('fails closed after the bounded immediate resync budget', async () => {
    const phases: string[] = []
    const controller = new AbortController()
    const resync = { kind: 'resync_required', cursor: '18' } as const

    await synchronizeAttention({
      transport: streamTransport([[resync], [resync], [resync], [resync]]),
      signal: controller.signal,
      onPhase: (phase) => phases.push(phase),
      onProjection: () => undefined,
    })

    expect(phases).toEqual(['connecting', 'resyncing', 'failed'])
  })

  it('resets the immediate resync budget after an authoritative projection', async () => {
    const phases: string[] = []
    const resync = { kind: 'resync_required', cursor: '18' } as const
    const recovered = { kind: 'snapshot', snapshot } as const

    await synchronizeAttention({
      transport: streamTransport([
        [resync],
        [recovered, resync],
        [recovered, resync],
        [recovered, resync],
        [recovered],
      ]),
      signal: new AbortController().signal,
      onPhase: (phase) => phases.push(phase),
      onProjection: () => undefined,
    })

    expect(phases.at(-1)).toBe('stale')
  })
})
