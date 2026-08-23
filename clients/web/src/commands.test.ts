import { describe, expect, it } from 'vitest'
import { invokeCommand } from './commands'
import { invokeProductCommand } from './productCommands'
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

  it('keeps server-window navigation available when filtering hides every row', () => {
    const navigated: Array<'first' | 'latest'> = []
    const context = {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      navigate: () => undefined,
      navigateTimelineWindow: (anchor: 'first' | 'latest') => navigated.push(anchor),
      openNavigation: () => undefined,
      openPalette: () => undefined,
      prepareFocusLayout: () => undefined,
      timelineWindowAvailable: true,
    }

    invokeProductCommand('selection.first', context)
    invokeProductCommand('selection.last', context)

    expect(navigated).toEqual(['first', 'latest'])
  })

  it('applies explicit settings through registered commands', () => {
    const context = {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      navigate: () => undefined,
      navigateTimelineWindow: () => undefined,
      openNavigation: () => undefined,
      openPalette: () => undefined,
      prepareFocusLayout: () => undefined,
      timelineWindowAvailable: false,
    }

    invokeProductCommand('layout.focus', context)
    invokeProductCommand('density.comfortable', context)
    invokeProductCommand('detail.results', context)
    invokeProductCommand('theme.light', context)

    expect(selectApp(store.getState())).toMatchObject({
      layout: 'focus',
      density: 'comfortable',
      detail: 'results',
      theme: 'light',
    })
  })
})
