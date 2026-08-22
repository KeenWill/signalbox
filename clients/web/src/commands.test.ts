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

  it('routes artifact actions through a registered command', () => {
    let invocations = 0

    invokeCommand('artifact.preview.expand', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      artifactAction: () => {
        invocations += 1
      },
    })

    expect(invocations).toBe(1)
  })

  it('persists timeline selections made by commands', () => {
    const persisted: string[] = []

    invokeCommand('selection.last', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: ['41', '42'],
      focusTimeline: () => undefined,
      onTimelineSelected: (eventSequence) => persisted.push(eventSequence),
    })

    expect(persisted).toEqual(['42'])
  })

  it('routes the first-item sequence to the owning window action when available', () => {
    let firstWindowRequests = 0

    invokeCommand('selection.first', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: ['41', '42'],
      focusTimeline: () => undefined,
      openFirstTimelineWindow: () => {
        firstWindowRequests += 1
      },
    })

    expect(firstWindowRequests).toBe(1)
  })
})
