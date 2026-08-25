import { afterEach, describe, expect, it, vi } from 'vitest'
import { commandById, invokeCommand } from './commands'
import { productCommandRegistry } from './productCommands'
import { actions, selectApp, store } from './state'

describe('command registry', () => {
  afterEach(() => vi.unstubAllGlobals())

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
    })

    expect(selectApp(store.getState()).selectedTimeline).toBe(timelineIds[0])
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

  it('applies an exact Settings preference through its registered command', () => {
    invokeCommand('theme.light', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
    })

    expect(selectApp(store.getState()).theme).toBe('light')
  })

  it('offers transcript detail only for transcript and Settings contexts', () => {
    const context = {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
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
      focusTimeline: () => undefined,
      paneSize: 320,
    }

    invokeCommand('pane.navigation.preview', context)

    expect(selectApp(store.getState()).paneSizes.navigation).toBe(320)
    expect(setItem).not.toHaveBeenCalled()

    invokeCommand('pane.navigation.resize', context)

    expect(setItem).toHaveBeenCalledOnce()
  })
})
