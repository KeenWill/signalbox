import { describe, expect, it } from 'vitest'
import { commandById, globalHotkeySequenceBindings, invokeCommand } from './commands'
import { actions, selectApp, store } from './state'

describe('command registry', () => {
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

  it('registers every displayed product navigation sequence', () => {
    expect(globalHotkeySequenceBindings).toEqual(
      expect.arrayContaining([
        { commandId: 'navigate.attention', sequence: ['G', 'A'] },
        { commandId: 'navigate.sessions', sequence: ['G', 'S'] },
        { commandId: 'navigate.settings', sequence: ['G', ','] },
      ]),
    )
  })

  it('registers Scenario Studio as product navigation', () => {
    let destination = ''

    invokeCommand('navigate.scenario', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      navigate: (path) => {
        destination = path
      },
    })

    expect(commandById('navigate.scenario').title).toBe('Go to Scenario Studio')
    expect(destination).toBe('/scenario/streaming')
  })

  it('unwinds a surface before returning focus to its root', () => {
    let focused = false
    invokeCommand('surface.escape', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => {
        focused = true
      },
      unwindSurface: () => true,
    })

    expect(focused).toBe(false)
  })
})
