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

  it('persists keyboard-driven selection through the shared callback', () => {
    const selected: string[] = []
    store.dispatch(actions.timelineSelected(null))

    invokeCommand('selection.next', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: ['41', '42'],
      focusTimeline: () => undefined,
      selectTimeline: (eventSequence) => selected.push(eventSequence),
    })

    expect(selectApp(store.getState()).selectedTimeline).toBe('41')
    expect(selected).toEqual(['41'])
  })

  it('routes first and latest commands to whole-window actions', () => {
    const anchors: Array<'first' | 'latest'> = []
    const context = {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: ['41', '42'],
      focusTimeline: () => undefined,
      openTimelineWindow: (anchor: 'first' | 'latest') => anchors.push(anchor),
    }

    invokeCommand('selection.first', context)
    invokeCommand('selection.last', context)

    expect(anchors).toEqual(['first', 'latest'])
  })
})
