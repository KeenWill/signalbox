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

  it('routes the expansion command through the registered timeline control', () => {
    const timelineIds = ['event-0'] as const
    let toggled = false
    store.dispatch(actions.timelineSelected(timelineIds[0]))

    invokeCommand('selection.toggleExpansion', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds,
      focusTimeline: () => undefined,
      toggleTimelineExpansion: () => {
        toggled = true
      },
    })

    expect(toggled).toBe(true)
  })
})
