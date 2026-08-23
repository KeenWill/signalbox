import { describe, expect, it } from 'vitest'
import { activityTime } from './AttentionSurface'
import { reduceAttentionEvent, synchronizeAttention } from './attention'
import type { WebAttentionSnapshot, WebAttentionStreamEvent } from './generated/web-contract.mjs'
import type { ProductTransport } from './product'

const sessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d'
const earlierSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6c'
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
  it('preserves timestamps outside the JavaScript date range', () => {
    expect(activityTime('9000000000000000')).toBe('9000000000000000')
  })

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
      summaries: [{ ...replacement, session_id: earlierSessionId }],
    })

    expect(reduction).toEqual({ kind: 'resync' })
  })

  it('ignores updates beyond the bounded page while advancing the cursor', () => {
    const reduction = reduceAttentionEvent(snapshot, {
      kind: 'update',
      cursor: '18',
      summaries: [{ ...replacement, session_id: anotherSessionId }],
    })

    expect(reduction).toEqual({
      kind: 'projection',
      snapshot: { ...snapshot, cursor: '18' },
    })
  })

  it('requests resynchronization instead of installing a regressing cursor', () => {
    const reduction = reduceAttentionEvent(snapshot, {
      kind: 'update',
      cursor: '16',
      summaries: [replacement],
    })

    expect(reduction).toEqual({ kind: 'resync' })
  })

  it('ignores an empty update at the current cursor', () => {
    const reduction = reduceAttentionEvent(snapshot, {
      kind: 'update',
      cursor: snapshot.cursor,
      summaries: [],
    })

    expect(reduction).toEqual({ kind: 'projection', snapshot })
  })

  it('requests resynchronization for replacements at the current cursor', () => {
    const reduction = reduceAttentionEvent(snapshot, {
      kind: 'update',
      cursor: snapshot.cursor,
      summaries: [replacement],
    })

    expect(reduction).toEqual({ kind: 'resync' })
  })

  it('requests resynchronization for duplicate identities in an update', () => {
    const reduction = reduceAttentionEvent(snapshot, {
      kind: 'update',
      cursor: '18',
      summaries: [replacement, { ...replacement, state: 'blocked' }],
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
      onProjection: (projection) => {
        projections.push(projection)
        return { snapshot: projection, accepted: true }
      },
    })

    expect(phases).toEqual(['connecting', 'live', 'resyncing', 'live', 'stale'])
    expect(projections).toEqual([snapshot, { ...snapshot, cursor: '19' }])
  })

  it('fails closed after the bounded immediate resync budget', async () => {
    const phases: string[] = []
    const controller = new AbortController()
    const resync = { kind: 'resync_required', cursor: '18' } as const
    const recovered = { kind: 'snapshot', snapshot } as const

    await synchronizeAttention({
      transport: streamTransport([
        [recovered, resync],
        [recovered, resync],
        [recovered, resync],
        [recovered, resync],
      ]),
      signal: controller.signal,
      onPhase: (phase) => phases.push(phase),
      onProjection: (projection) => ({ snapshot: projection, accepted: true }),
    })

    expect(phases.at(-1)).toBe('failed')
  })

  it('fails closed when a follow response does not begin with a snapshot', async () => {
    const phases: string[] = []

    await synchronizeAttention({
      transport: streamTransport([[{ kind: 'update', cursor: '18', summaries: [replacement] }]]),
      signal: new AbortController().signal,
      onPhase: (phase) => phases.push(phase),
      onProjection: (projection) => ({ snapshot: projection, accepted: true }),
    })

    expect(phases).toEqual(['connecting', 'failed'])
  })

  it('resets the immediate resync budget after accepted incremental progress', async () => {
    const phases: string[] = []
    const resync = { kind: 'resync_required', cursor: '18' } as const
    const recovered = { kind: 'snapshot', snapshot } as const
    const progressed = { kind: 'update', cursor: '18', summaries: [replacement] } as const

    await synchronizeAttention({
      transport: streamTransport([
        [recovered, resync],
        [recovered, progressed, resync],
        [{ kind: 'snapshot', snapshot: { ...snapshot, cursor: '19' } }],
      ]),
      signal: new AbortController().signal,
      onPhase: (phase) => phases.push(phase),
      onProjection: (projection) => ({ snapshot: projection, accepted: true }),
    })

    expect(phases.at(-1)).toBe('stale')
  })

  it('preserves the immediate resync budget across replacement snapshots', async () => {
    const phases: string[] = []
    const resync = { kind: 'resync_required', cursor: '18' } as const
    const recovered = { kind: 'snapshot', snapshot } as const

    await synchronizeAttention({
      transport: streamTransport([
        [recovered, resync],
        [recovered, resync],
        [recovered, resync],
        [recovered, resync],
      ]),
      signal: new AbortController().signal,
      onPhase: (phase) => phases.push(phase),
      onProjection: (projection) => ({ snapshot: projection, accepted: true }),
    })

    expect(phases.at(-1)).toBe('failed')
  })

  it('uses the projection accepted by the cache as the follower baseline', async () => {
    const newerSnapshot = { ...snapshot, cursor: '19' }
    const projections: WebAttentionSnapshot[] = []

    await synchronizeAttention({
      transport: streamTransport([
        [
          { kind: 'snapshot', snapshot },
          { kind: 'update', cursor: '20', summaries: [replacement] },
        ],
      ]),
      signal: new AbortController().signal,
      onPhase: () => undefined,
      onProjection: (projection) => {
        const accepted =
          BigInt(projection.cursor) < BigInt(newerSnapshot.cursor) ? newerSnapshot : projection
        projections.push(accepted)
        return { snapshot: accepted, accepted: accepted === projection }
      },
    })

    expect(projections).toEqual([
      newerSnapshot,
      { ...newerSnapshot, cursor: '20', summaries: [replacement] },
    ])
  })

  it('does not reset the resync budget for rejected follower snapshots', async () => {
    const phases: string[] = []
    const staleSnapshot = { kind: 'snapshot', snapshot } as const
    const duplicateUpdate = { kind: 'update', cursor: '19', summaries: [replacement] } as const
    const newerSnapshot = { ...snapshot, cursor: '19' }

    await synchronizeAttention({
      transport: streamTransport([
        [staleSnapshot, duplicateUpdate],
        [staleSnapshot, duplicateUpdate],
        [staleSnapshot, duplicateUpdate],
        [staleSnapshot, duplicateUpdate],
      ]),
      signal: new AbortController().signal,
      onPhase: (phase) => phases.push(phase),
      onProjection: (projection) => ({
        snapshot: newerSnapshot,
        accepted: projection.cursor === newerSnapshot.cursor,
      }),
    })

    expect(phases.at(-1)).toBe('failed')
  })
})
