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

  it('runs available continuation actions through stable command identities', () => {
    const relationships: Array<'resume' | 'fork'> = []
    const context = {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      canContinueImport: true,
      continueImport: (relationship: 'resume' | 'fork') => relationships.push(relationship),
    }

    invokeCommand('imports.continue.resume', context)
    invokeCommand('imports.continue.fork', context)

    expect(relationships).toEqual(['resume', 'fork'])
  })

  it('does not run an unavailable continuation command', () => {
    const relationships: Array<'resume' | 'fork'> = []

    invokeCommand('imports.continue.resume', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      canContinueImport: false,
      continueImport: (relationship) => relationships.push(relationship),
    })

    expect(relationships).toEqual([])
  })
})
