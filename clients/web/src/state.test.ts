import { describe, expect, it } from 'vitest'
import {
  MAX_LOGICAL_POSITION_KEY_BYTES,
  MAX_LOGICAL_POSITION_VALUE_BYTES,
  serializeBrowserPreferences,
} from './preferences'
import { actions, selectApp, store } from './state'

describe('application state', () => {
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
    for (let index = 0; index < 128; index += 1) {
      store.dispatch(
        actions.logicalPositionRecorded({
          sessionId: `recent-${index}`,
          position: `cursor-${index}`,
        }),
      )
    }

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
    for (let index = 0; index < 128; index += 1) {
      store.dispatch(
        actions.logicalPositionRecorded({
          sessionId: `escaped-${index}`,
          position: '\0'.repeat(MAX_LOGICAL_POSITION_VALUE_BYTES),
        }),
      )
    }

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
