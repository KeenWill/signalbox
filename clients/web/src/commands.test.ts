import { describe, expect, it } from 'vitest'
import { commandById, invokeCommand } from './commands'
import { actions, selectApp, store } from './state'

describe('command registry', () => {
  it('selects the first timeline item when next starts from a missing selection', () => {
    const timelineIds = ['event-0', 'event-1'] as const
    store.dispatch(actions.timelineSelected('filtered-out-event'))

    invokeCommand('selection.next', {
      dispatch: store.dispatch,
      getState: store.getState,
      scenarioSurface: true,
      timelineIds,
      focusTimeline: () => undefined,
    })

    expect(selectApp(store.getState()).selectedTimeline).toBe(timelineIds[0])
  })

  it('keeps keyboard help available on an empty scenario surface', () => {
    const help = commandById('help.open')
    expect(
      help.available({
        dispatch: store.dispatch,
        getState: store.getState,
        scenarioSurface: true,
        timelineIds: [],
        focusTimeline: () => undefined,
      }),
    ).toBe(true)
  })

  it('keeps scenario keyboard help out of product surfaces', () => {
    const help = commandById('help.open')
    expect(
      help.available({
        dispatch: store.dispatch,
        getState: store.getState,
        scenarioSurface: false,
        timelineIds: [],
        focusTimeline: () => undefined,
      }),
    ).toBe(false)
  })

  it('keeps product navigation bindings unavailable on scenario surfaces', () => {
    const navigate = commandById('navigate.sessions')
    expect(
      navigate.available({
        dispatch: store.dispatch,
        getState: store.getState,
        scenarioSurface: true,
        timelineIds: [],
        focusTimeline: () => undefined,
      }),
    ).toBe(false)
  })
})
