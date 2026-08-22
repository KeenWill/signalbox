import { describe, expect, it } from 'vitest'
import { globalHotkeySequenceBindings, invokeCommand } from './commands'
import { actions, selectApp, store } from './state'

describe('command registry', () => {
  it('registers every advertised product navigation sequence', () => {
    expect(globalHotkeySequenceBindings).toEqual(
      expect.arrayContaining([
        { commandId: 'navigate.attention', sequence: ['G', 'A'] },
        { commandId: 'navigate.sessions', sequence: ['G', 'S'] },
        { commandId: 'navigate.activity', sequence: ['G', 'T'] },
        { commandId: 'navigate.settings', sequence: ['G', ','] },
      ]),
    )
  })

  it('selects the first timeline item when next starts from a missing selection', () => {
    const timelineIds = ['event-0', 'event-1'] as const
    store.dispatch(actions.timelineSelected('filtered-out-event'))

    invokeCommand('selection.next', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds,
      focusTimeline: () => undefined,
    })

    expect(selectApp(store.getState()).selectedTimeline).toBe(timelineIds[0])
  })
})
