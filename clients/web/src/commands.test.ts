import { afterEach, describe, expect, it, vi } from 'vitest'
import { commandById, globalHotkeySequenceBindings, invokeCommand } from './commands'
import { productCommandRegistry } from './productCommands'
import { actions, selectApp, store } from './state'

describe('command registry', () => {
  afterEach(() => vi.unstubAllGlobals())

  it('registers every advertised product navigation sequence', () => {
    expect(globalHotkeySequenceBindings).toEqual(
      expect.arrayContaining([
        { commandId: 'navigate.attention', sequence: ['G', 'A'] },
        { commandId: 'navigate.sessions', sequence: ['G', 'S'] },
        { commandId: 'navigate.settings', sequence: ['G', ','] },
      ]),
    )
  })

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
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => undefined,
    })

    expect(selectApp(store.getState()).theme).toBe('dark')
  })

  it('routes pane resizing through parameterized central commands', () => {
    invokeCommand('pane.navigation.resize', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => undefined,
      paneSize: 320,
    })
    invokeCommand('pane.inspector.resize', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
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
      artifactPreviewIds: [],
      artifactOriginalIds: [],
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
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => undefined,
      searchAvailable: false,
      focusSearch: () => undefined,
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

  it('registers every displayed product navigation sequence', () => {
    expect(globalHotkeySequenceBindings).toEqual(
      expect.arrayContaining([
        { commandId: 'navigate.attention', sequence: ['G', 'A'] },
        { commandId: 'navigate.sessions', sequence: ['G', 'S'] },
        { commandId: 'navigate.settings', sequence: ['G', ','] },
      ]),
    )
  })

  it('registers Scenario Studio as product navigation', () => {
    let destination = ''

    invokeCommand('navigate.scenario', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => undefined,
      navigate: (path) => {
        destination = path
      },
    })

    expect(commandById('navigate.scenario').title).toBe('Go to Scenario Studio')
    expect(destination).toBe('/scenario/streaming')
  })

  it('unwinds a surface before returning focus to its root', () => {
    let focused = false
    invokeCommand('surface.escape', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => {
        focused = true
      },
      unwindSurface: () => true,
    })

    expect(focused).toBe(false)
  })

  it('selects the next immutable imported frontier through the command registry', () => {
    const importEntryIds = ['import-entry-1', 'import-entry-2'] as const
    let selectedImportEntry: (typeof importEntryIds)[number] = importEntryIds[0]

    invokeCommand('imports.entry.next', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
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
      artifactPreviewIds: [],
      artifactOriginalIds: [],
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
      artifactPreviewIds: [],
      artifactOriginalIds: [],
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
      artifactPreviewIds: [],
      artifactOriginalIds: [],
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
      artifactPreviewIds: [],
      artifactOriginalIds: [],
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
      artifactPreviewIds: [],
      artifactOriginalIds: [],
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
      artifactPreviewIds: [],
      artifactOriginalIds: [],
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
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      timelineWindowAvailable: true,
      focusTimeline: () => undefined,
      loadTimelineWindow: (anchor: 'first' | 'latest') => loaded.push(anchor),
    }

    invokeCommand('selection.first', context)
    invokeCommand('selection.last', context)

    expect(loaded).toEqual(['first', 'latest'])
  })

  it('applies an exact Settings preference through its registered command', () => {
    invokeCommand('theme.light', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => undefined,
    })

    expect(selectApp(store.getState()).theme).toBe('light')
  })

  it('offers transcript detail only for transcript and Settings contexts', () => {
    const context = {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => undefined,
    }

    expect(commandById('detail.full').available(context)).toBe(false)
    expect(
      commandById('detail.full').available({ ...context, configuresTranscriptDetail: true }),
    ).toBe(true)
    expect(commandById('detail.full').available({ ...context, timelineIds: ['event-0'] })).toBe(
      true,
    )
  })

  it('previews pane sizes without writing preferences until commit', () => {
    const setItem = vi.fn()
    vi.stubGlobal('localStorage', { setItem })
    const context = {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => undefined,
      paneSize: 320,
    }

    invokeCommand('pane.navigation.preview', context)

    expect(selectApp(store.getState()).paneSizes.navigation).toBe(320)
    expect(setItem).not.toHaveBeenCalled()

    invokeCommand('pane.navigation.resize', context)

    expect(setItem).toHaveBeenCalledOnce()
  })

  it('withholds the artifact inspector until a surface owns an opener', () => {
    const artifact = commandById('artifact.open')
    const base = {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => undefined,
    }

    expect(artifact.available(base)).toBe(false)
    expect(artifact.available({ ...base, openArtifactInspector: () => undefined })).toBe(true)
  })

  it('routes artifact inspection through the owning surface opener', () => {
    let opened = 0

    invokeCommand('artifact.open', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => undefined,
      openArtifactInspector: () => {
        opened += 1
      },
    })

    expect(opened).toBe(1)
  })

  it('keeps the artifact inspector reachable from product surfaces', () => {
    const productCommandIds: readonly string[] = productCommandRegistry.map((command) => command.id)

    expect(productCommandIds).toContain('artifact.open')
  })
})
