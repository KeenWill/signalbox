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

  it('sets transcript detail through the registry in Settings without a timeline', () => {
    invokeCommand('detail.results', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      transcriptPreferences: true,
    })

    expect(selectApp(store.getState()).detail).toBe('results')
  })

  it('sets exact presentation preferences through registered commands', () => {
    const context = {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      presentationPreferences: true,
    }

    invokeCommand('layout.focus', context)
    invokeCommand('density.comfortable', context)
    invokeCommand('theme.light', context)

    expect(selectApp(store.getState())).toMatchObject({
      layout: 'focus',
      density: 'comfortable',
      theme: 'light',
    })

    invokeCommand('preferences.reset', context)

    expect(selectApp(store.getState())).toMatchObject({
      layout: 'workbench',
      density: 'compact',
      detail: 'condensed',
      theme: 'dark',
    })
  })
})
