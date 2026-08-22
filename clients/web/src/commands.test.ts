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
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => undefined,
    })

    expect(selectApp(store.getState()).selectedTimeline).toBe(timelineIds[0])
  })

  it('lets the registered artifact command own expansion state', () => {
    store.dispatch(actions.artifactSelected('artifact-1'))

    invokeCommand('artifact.preview.expand', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds: ['artifact-1'],
      artifactOriginalIds: [],
      focusTimeline: () => undefined,
    })

    expect(selectApp(store.getState()).expandedArtifacts['artifact-1']).toBe(true)
  })
})
