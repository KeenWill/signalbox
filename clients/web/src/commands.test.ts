import { describe, expect, it } from 'vitest'
import { invokeCommand } from './commands'
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

  it('applies an exact Settings preference through its registered command', () => {
    invokeCommand('theme.light', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
    })

    expect(selectApp(store.getState()).theme).toBe('light')
  })

  it('applies a pane size through its registered parameterized command', () => {
    invokeCommand('pane.navigation.resize', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      paneSize: 300,
    })

    expect(selectApp(store.getState()).paneSizes.navigation).toBe(300)
  })
})
