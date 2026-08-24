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
      canSelectImportEntry: true,
      selectImportEntry: (id) => {
        selectedImportEntry = id as (typeof importEntryIds)[number]
      },
    })

    expect(selectedImportEntry).toBe(importEntryIds[1])
  })

  it('opens keyboard help for standalone Imports without timeline rows', () => {
    invokeCommand('help.open', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      keyboardHelpAvailable: true,
    })

    expect(selectApp(store.getState()).overlay).toBe('help')
    store.dispatch(actions.overlaySet(null))
  })

  it('does not run imported-entry navigation while exact recovery freezes selection', () => {
    const selected: string[] = []

    invokeCommand('imports.entry.next', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      importEntryIds: ['import-entry-1', 'import-entry-2'],
      selectedImportEntry: 'import-entry-1',
      canSelectImportEntry: false,
      selectImportEntry: (id) => selected.push(id),
    })

    expect(selected).toEqual([])
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

  it('resizes workbench panes through the command registry', () => {
    const requestedPaneSizes = { navigation: 280, inspector: 360 }

    invokeCommand('panes.resize', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      presentationPreferences: true,
      requestedPaneSizes,
    })

    expect(selectApp(store.getState()).paneSizes).toEqual(requestedPaneSizes)
  })

  it('moves focus before the explicit focus layout hides navigation', () => {
    const focused: string[] = []
    store.dispatch(actions.layoutSet('workbench'))

    invokeCommand('layout.focus', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      focusPrimaryWhenNavigationHides: () => focused.push('primary'),
      presentationPreferences: true,
    })

    expect(focused).toEqual(['primary'])
    expect(selectApp(store.getState()).layout).toBe('focus')
    store.dispatch(actions.layoutSet('workbench'))
  })

  it('falls back to scenario timeline focus before hiding navigation', () => {
    const focused: string[] = []
    store.dispatch(actions.layoutSet('workbench'))

    invokeCommand('layout.toggle', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: ['event-0'],
      focusTimeline: () => focused.push('timeline'),
    })

    expect(focused).toEqual(['timeline'])
    expect(selectApp(store.getState()).layout).toBe('focus')
    store.dispatch(actions.layoutSet('workbench'))
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

  it('runs available exact-retry recovery actions through stable command identities', () => {
    const recoveryActions: string[] = []
    const context = {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      canRetryImport: true,
      retryImport: () => recoveryActions.push('retry'),
      canAbandonImport: true,
      abandonImport: () => recoveryActions.push('abandon'),
    }

    invokeCommand('imports.continue.retry', context)
    invokeCommand('imports.continue.abandon', context)

    expect(recoveryActions).toEqual(['retry', 'abandon'])
  })
})
