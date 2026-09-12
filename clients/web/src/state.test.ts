import { beforeEach, describe, expect, it } from 'vitest'
import {
  MAX_BROWSER_PREFERENCES_BYTES,
  MAX_LOGICAL_POSITION_KEY_BYTES,
  MAX_LOGICAL_POSITION_VALUE_BYTES,
  serializeBrowserPreferences,
} from './preferences'
import {
  actions,
  createAppStore,
  MAX_PENDING_SESSION_INPUTS,
  selectApp,
  selectSessionInputCapacityReached,
} from './state'

let testStore = createAppStore()

const recordLogicalPositionFixture = (
  prefix: string,
  count: number,
  position: (index: number) => string,
) => {
  const sessionIds = Array.from({ length: count }, (_, index) => `${prefix}-${index}`)
  for (const [index, sessionId] of sessionIds.entries()) {
    testStore.dispatch(actions.logicalPositionRecorded({ sessionId, position: position(index) }))
  }
  return sessionIds
}

describe('application state', () => {
  beforeEach(() => {
    testStore = createAppStore()
  })

  it('releases retired original loads without reviving them on a late completion', () => {
    const retired = 'product-artifact:1:retired'
    const current = 'product-artifact:2:current'
    testStore.dispatch(actions.artifactOriginalRequested(retired))
    testStore.dispatch(actions.artifactOriginalRequested(current))
    testStore.dispatch(actions.artifactOriginalReleased(retired))
    testStore.dispatch(actions.artifactOriginalSettled({ id: retired, result: 'loaded' }))

    expect(selectApp(testStore.getState()).originalArtifacts).toEqual({ [current]: 'loading' })
  })

  it('records logical-position fixtures in declared order', () => {
    const sessionIds = recordLogicalPositionFixture('fixture', 2, (index) => String(index + 1))

    const positions = selectApp(testStore.getState()).lastLogicalPositions
    expect(sessionIds).toEqual(['fixture-0', 'fixture-1'])
    expect(positions['fixture-0']).toBe('1')
    expect(positions['fixture-1']).toBe('2')
  })

  it('rejects oversized logical-position keys and malformed positions before persistence', () => {
    const oversizedSession = 'é'.repeat(MAX_LOGICAL_POSITION_KEY_BYTES)
    const oversizedPosition = 'é'.repeat(MAX_LOGICAL_POSITION_VALUE_BYTES)

    testStore.dispatch(
      actions.logicalPositionRecorded({ sessionId: oversizedSession, position: '7' }),
    )
    testStore.dispatch(
      actions.logicalPositionRecorded({
        sessionId: 'oversized-position',
        position: oversizedPosition,
      }),
    )

    expect(selectApp(testStore.getState()).lastLogicalPositions[oversizedSession]).toBeUndefined()
    expect(
      selectApp(testStore.getState()).lastLogicalPositions['oversized-position'],
    ).toBeUndefined()
  })

  it('rejects logical positions that are not positive decimal sequences', () => {
    testStore.dispatch(
      actions.logicalPositionRecorded({ sessionId: 'malformed-position', position: 'cursor' }),
    )
    testStore.dispatch(
      actions.logicalPositionRecorded({ sessionId: 'zero-position', position: '0' }),
    )
    testStore.dispatch(
      actions.logicalPositionRecorded({
        sessionId: 'overflow-position',
        position: '18446744073709551616',
      }),
    )

    const positions = selectApp(testStore.getState()).lastLogicalPositions
    expect(Object.hasOwn(positions, 'malformed-position')).toBe(false)
    expect(Object.hasOwn(positions, 'zero-position')).toBe(false)
    expect(Object.hasOwn(positions, 'overflow-position')).toBe(false)
  })

  it('retains a re-recorded session at the capacity boundary', () => {
    recordLogicalPositionFixture('recent', 128, (index) => String(index + 1))

    testStore.dispatch(actions.logicalPositionRecorded({ sessionId: 'recent-0', position: '999' }))
    testStore.dispatch(
      actions.logicalPositionRecorded({ sessionId: 'recent-overflow', position: '7' }),
    )

    const positions = selectApp(testStore.getState()).lastLogicalPositions
    expect(positions['recent-0']).toBe('999')
    expect(positions['recent-1']).toBeUndefined()
    expect(positions['recent-overflow']).toBe('7')
  })

  it('rejects session IDs with unordered plain-object key semantics', () => {
    testStore.dispatch(actions.logicalPositionRecorded({ sessionId: '1', position: '1' }))
    testStore.dispatch(actions.logicalPositionRecorded({ sessionId: '__proto__', position: '2' }))

    const positions = selectApp(testStore.getState()).lastLogicalPositions
    expect(Object.hasOwn(positions, '1')).toBe(false)
    expect(Object.hasOwn(positions, '__proto__')).toBe(false)
  })

  it('keeps a full logical-position capacity under the serialized preference ceiling', () => {
    recordLogicalPositionFixture(
      'é'.repeat((MAX_LOGICAL_POSITION_KEY_BYTES - 8) / 2),
      128,
      () => '18446744073709551615',
    )

    const app = selectApp(testStore.getState())
    const serialized = serializeBrowserPreferences({
      navigationCollapsed: app.navigationCollapsed,
      layout: app.layout,
      density: app.density,
      detail: app.detail,
      theme: app.theme,
      paneSizes: app.paneSizes,
      lastLogicalPositions: app.lastLogicalPositions,
    })
    expect(serialized).not.toBeNull()
    expect(new TextEncoder().encode(serialized ?? '').length).toBeLessThanOrEqual(
      MAX_BROWSER_PREFERENCES_BYTES,
    )
  })
})

describe('retained input admission', () => {
  it('refuses new sessions at capacity, preserves retries, and frees capacity only on confirmation', () => {
    const store = createAppStore()
    const requests = Array.from({ length: MAX_PENDING_SESSION_INPUTS + 1 }, (_, index) => ({
      sessionId: `session-${index}`,
      input: { command_id: `command-${index}`, message: `Retain message ${index}` },
    }))
    for (const request of requests) {
      store.dispatch(actions.sessionInputStarted(request))
      store.dispatch(
        actions.sessionInputSettled({
          sessionId: request.sessionId,
          commandId: request.input.command_id,
          confirmed: false,
        }),
      )
    }
    const first = requests[0]
    const extra = requests[MAX_PENDING_SESSION_INPUTS]
    if (first === undefined || extra === undefined) throw new Error('Missing capacity fixtures')
    expect(selectSessionInputCapacityReached(store.getState())).toBe(true)
    expect(store.getState().app.pendingSessionInputs[extra.sessionId]).toBeUndefined()
    store.dispatch(actions.sessionInputStarted(first))
    expect(store.getState().app.pendingSessionInputs[first.sessionId]).toEqual({
      input: first.input,
      phase: 'sending',
    })
    store.dispatch(
      actions.sessionInputSettled({
        sessionId: first.sessionId,
        commandId: first.input.command_id,
        confirmed: false,
      }),
    )
    expect(selectSessionInputCapacityReached(store.getState())).toBe(true)
    store.dispatch(
      actions.sessionInputSettled({
        sessionId: first.sessionId,
        commandId: first.input.command_id,
        confirmed: true,
      }),
    )
    expect(selectSessionInputCapacityReached(store.getState())).toBe(false)
    store.dispatch(actions.sessionInputStarted(extra))
    expect(store.getState().app.pendingSessionInputs[extra.sessionId]?.input).toEqual(extra.input)
    expect(Object.keys(store.getState().app.pendingSessionInputs)).toHaveLength(
      MAX_PENDING_SESSION_INPUTS,
    )
  })
})
