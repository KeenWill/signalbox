import { beforeEach, describe, expect, it } from 'vitest'
import {
  MAX_BROWSER_PREFERENCES_BYTES,
  MAX_LOGICAL_POSITION_KEY_BYTES,
  MAX_LOGICAL_POSITION_VALUE_BYTES,
  serializeBrowserPreferences,
} from './preferences'
import { actions, createAppStore, selectApp } from './state'

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

  it('records logical-position fixtures in declared order', () => {
    const sessionIds = recordLogicalPositionFixture('fixture', 2, (index) => `cursor-${index}`)

    const positions = selectApp(testStore.getState()).lastLogicalPositions
    expect(sessionIds).toEqual(['fixture-0', 'fixture-1'])
    expect(positions['fixture-0']).toBe('cursor-0')
    expect(positions['fixture-1']).toBe('cursor-1')
  })

  it('rejects oversized logical-position keys and values before persistence', () => {
    const oversizedSession = 'é'.repeat(MAX_LOGICAL_POSITION_KEY_BYTES)
    const oversizedPosition = 'é'.repeat(MAX_LOGICAL_POSITION_VALUE_BYTES)

    testStore.dispatch(
      actions.logicalPositionRecorded({ sessionId: oversizedSession, position: 'cursor' }),
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

  it('retains a re-recorded session at the capacity boundary', () => {
    recordLogicalPositionFixture('recent', 128, (index) => `cursor-${index}`)

    testStore.dispatch(
      actions.logicalPositionRecorded({ sessionId: 'recent-0', position: 'refreshed' }),
    )
    testStore.dispatch(
      actions.logicalPositionRecorded({ sessionId: 'recent-overflow', position: 'cursor' }),
    )

    const positions = selectApp(testStore.getState()).lastLogicalPositions
    expect(positions['recent-0']).toBe('refreshed')
    expect(positions['recent-1']).toBeUndefined()
    expect(positions['recent-overflow']).toBe('cursor')
  })

  it('rejects session IDs with unordered plain-object key semantics', () => {
    testStore.dispatch(actions.logicalPositionRecorded({ sessionId: '1', position: 'numeric' }))
    testStore.dispatch(
      actions.logicalPositionRecorded({ sessionId: '__proto__', position: 'prototype' }),
    )

    const positions = selectApp(testStore.getState()).lastLogicalPositions
    expect(Object.hasOwn(positions, '1')).toBe(false)
    expect(Object.hasOwn(positions, '__proto__')).toBe(false)
  })

  it('rejects the first logical position above the serialized preference ceiling', () => {
    recordLogicalPositionFixture('escaped', 128, () =>
      '\0'.repeat(MAX_LOGICAL_POSITION_VALUE_BYTES),
    )

    const app = selectApp(testStore.getState())
    const serialized = serializeBrowserPreferences({
      layout: app.layout,
      density: app.density,
      detail: app.detail,
      theme: app.theme,
      paneSizes: app.paneSizes,
      remoteMedia: app.remoteMedia,
      lastLogicalPositions: app.lastLogicalPositions,
      keyOverrides: app.keyOverrides,
    })
    expect(serialized).not.toBeNull()
    expect(new TextEncoder().encode(serialized ?? '').length).toBeLessThanOrEqual(
      MAX_BROWSER_PREFERENCES_BYTES,
    )
    expect(app.lastLogicalPositions['escaped-41']).toBeDefined()
    expect(app.lastLogicalPositions['escaped-42']).toBeUndefined()
    expect(
      serializeBrowserPreferences({
        layout: app.layout,
        density: app.density,
        detail: app.detail,
        theme: app.theme,
        paneSizes: app.paneSizes,
        remoteMedia: app.remoteMedia,
        lastLogicalPositions: {
          ...app.lastLogicalPositions,
          'escaped-42': '\0'.repeat(MAX_LOGICAL_POSITION_VALUE_BYTES),
        },
        keyOverrides: app.keyOverrides,
      }),
    ).toBeNull()
  })
})
