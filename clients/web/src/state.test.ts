import { describe, expect, it } from 'vitest'
import { MAX_LOGICAL_POSITION_KEY_BYTES, MAX_LOGICAL_POSITION_VALUE_BYTES } from './preferences'
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
})
