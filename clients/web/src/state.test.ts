import { describe, expect, it } from 'vitest'
import {
  MAX_LOGICAL_POSITION_KEY_BYTES,
  MAX_LOGICAL_POSITION_VALUE_BYTES,
  serializeBrowserPreferences,
} from './preferences'
import { actions, selectApp, store } from './state'

const recordLogicalPositionFixture = (
  prefix: string,
  count: number,
  position: (index: number) => string,
) => {
  const sessionIds = Array.from({ length: count }, (_, index) => `${prefix}-${index}`)
  for (const [index, sessionId] of sessionIds.entries()) {
    store.dispatch(actions.logicalPositionRecorded({ sessionId, position: position(index) }))
  }
  return sessionIds
}

describe('application state', () => {
  it('records logical-position fixtures in declared order', () => {
    const sessionIds = recordLogicalPositionFixture('fixture', 2, (index) => `cursor-${index}`)

    const positions = selectApp(store.getState()).lastLogicalPositions
    expect(sessionIds).toEqual(['fixture-0', 'fixture-1'])
    expect(positions['fixture-0']).toBe('cursor-0')
    expect(positions['fixture-1']).toBe('cursor-1')
  })

  it('rejects oversized logical-position keys and values before persistence', () => {
    const oversizedSession = 'é'.repeat(MAX_LOGICAL_POSITION_KEY_BYTES)
    const oversizedPosition = 'é'.repeat(MAX_LOGICAL_POSITION_VALUE_BYTES)

    store.dispatch(
      actions.logicalPositionRecorded({ sessionId: oversizedSession, position: 'cursor' }),
    )
    store.dispatch(
      actions.logicalPositionRecorded({
        sessionId: 'oversized-position',
        position: oversizedPosition,
      }),
    )

    expect(selectApp(store.getState()).lastLogicalPositions[oversizedSession]).toBeUndefined()
    expect(selectApp(store.getState()).lastLogicalPositions['oversized-position']).toBeUndefined()
  })

  it('retains a re-recorded session at the capacity boundary', () => {
    recordLogicalPositionFixture('recent', 128, (index) => `cursor-${index}`)

    store.dispatch(
      actions.logicalPositionRecorded({ sessionId: 'recent-0', position: 'refreshed' }),
    )
    store.dispatch(
      actions.logicalPositionRecorded({ sessionId: 'recent-overflow', position: 'cursor' }),
    )

    const positions = selectApp(store.getState()).lastLogicalPositions
    expect(positions['recent-0']).toBe('refreshed')
    expect(positions['recent-1']).toBeUndefined()
    expect(positions['recent-overflow']).toBe('cursor')
  })

  it('rejects logical positions that would exceed the serialized preference ceiling', () => {
    recordLogicalPositionFixture('escaped', 128, () =>
      '\0'.repeat(MAX_LOGICAL_POSITION_VALUE_BYTES),
    )

    const app = selectApp(store.getState())
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
    expect(
      Object.keys(app.lastLogicalPositions).filter((sessionId) => sessionId.startsWith('escaped-')),
    ).not.toHaveLength(128)
  })
})
