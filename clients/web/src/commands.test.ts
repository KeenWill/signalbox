import { describe, expect, it } from 'vitest'
import { invokeCommand } from './commands'
import { productCommandRegistry } from './productCommands'
import { actions, selectApp, store } from './state'

describe('command registry', () => {
  it('replaces scenario navigation with product navigation', () => {
    const productCommandIds: readonly string[] = productCommandRegistry.map((command) => command.id)
    expect(productCommandIds.filter((id) => id === 'navigation.open')).toHaveLength(1)
    expect(productCommandRegistry.find((command) => command.id === 'navigation.open')?.title).toBe(
      'Open product navigation',
    )
  })

  it('registers preference reset as a central command', () => {
    store.dispatch(actions.themeSet('light'))

    invokeCommand('preferences.reset', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
    })

    expect(selectApp(store.getState()).theme).toBe('dark')
  })

  it('routes pane resizing through parameterized central commands', () => {
    invokeCommand('pane.navigation.resize', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      paneSize: 320,
    })
    invokeCommand('pane.inspector.resize', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      paneSize: 440,
    })

    expect(selectApp(store.getState()).paneSizes).toEqual({ navigation: 320, inspector: 440 })
  })

  it('routes exact session opening through a parameterized central command', () => {
    const opened: string[] = []

    invokeCommand('session.open', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      sessionId: '00000000-0000-0000-0000-000000000991',
      openSession: (sessionId) => opened.push(sessionId),
    })

    expect(opened).toEqual(['00000000-0000-0000-0000-000000000991'])
  })

  it('selects the first timeline item when next starts from a missing selection', () => {
    const timelineIds = ['event-0', 'event-1'] as const
    store.dispatch(actions.timelineSelected('filtered-out-event'))

    invokeCommand('selection.next', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds,
      focusTimeline: () => undefined,
      searchAvailable: false,
      focusSearch: () => undefined,
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

  it('delegates first and latest commands to the owning server-window loader', () => {
    const loaded: Array<'first' | 'latest'> = []
    const context = {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: ['41', '42'],
      focusTimeline: () => undefined,
      loadTimelineWindow: (anchor: 'first' | 'latest') => loaded.push(anchor),
    }

    invokeCommand('selection.first', context)
    invokeCommand('selection.last', context)

    expect(loaded).toEqual(['first', 'latest'])
  })

  it('routes selected timeline expansion through the central command', () => {
    let toggles = 0
    store.dispatch(actions.timelineSelected('42'))

    invokeCommand('selection.toggleExpansion', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: ['41', '42'],
      focusTimeline: () => undefined,
      toggleTimelineExpansion: () => {
        toggles += 1
      },
    })

    expect(toggles).toBe(1)
  })

  it('keeps server-window boundary commands available when Results has no visible rows', () => {
    const loaded: Array<'first' | 'latest'> = []
    const context = {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      timelineWindowAvailable: true,
      focusTimeline: () => undefined,
      loadTimelineWindow: (anchor: 'first' | 'latest') => loaded.push(anchor),
    }

    invokeCommand('selection.first', context)
    invokeCommand('selection.last', context)

    expect(loaded).toEqual(['first', 'latest'])
  })
})
