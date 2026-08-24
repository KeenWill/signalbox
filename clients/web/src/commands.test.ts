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

  it('selects the next immutable imported frontier through the command registry', () => {
    const importEntryIds = ['import-entry-1', 'import-entry-2'] as const
    let selectedImportEntry: (typeof importEntryIds)[number] = importEntryIds[0]

    invokeCommand('imports.entry.next', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      importEntryIds,
      selectedImportEntry,
      selectImportEntry: (id) => {
        selectedImportEntry = id as (typeof importEntryIds)[number]
      },
    })

    expect(selectedImportEntry).toBe(importEntryIds[1])
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

  it('persists timeline selections made by commands', () => {
    const persisted: string[] = []

    invokeCommand('selection.last', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: ['41', '42'],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
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
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => undefined,
      openFirstTimelineWindow: () => {
        firstWindowRequests += 1
      },
    })

    expect(firstWindowRequests).toBe(1)
  })

  it('routes the last-item hotkey to the owning latest-window action when available', () => {
    let latestWindowRequests = 0

    invokeCommand('selection.last', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: ['41', '42'],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => undefined,
      openLatestTimelineWindow: () => {
        latestWindowRequests += 1
      },
    })

    expect(latestWindowRequests).toBe(1)
  })

  it('keeps the latest-window action available when filtering hides every row', () => {
    let latestWindowRequests = 0

    invokeCommand('selection.last', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => undefined,
      openLatestTimelineWindow: () => {
        latestWindowRequests += 1
      },
    })

    expect(latestWindowRequests).toBe(1)
  })

  it('lets the registered artifact command own selection state', () => {
    invokeCommand('artifact.select', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      artifactSelectionTarget: 'artifact-2',
      focusTimeline: () => undefined,
    })

    expect(selectApp(store.getState()).selectedArtifact).toBe('artifact-2')
  })
})
