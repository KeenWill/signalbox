import { afterEach, describe, expect, it, vi } from 'vitest'
import { commandById, invokeCommand } from './commands'
import { actions, selectApp, store } from './state'

describe('command registry', () => {
  afterEach(() => vi.unstubAllGlobals())

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

  it('applies a pane size through its registered parameterized command', () => {
    invokeCommand('pane.navigation.resize', {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      paneSize: 300,
    })

    expect(selectApp(store.getState()).paneSizes.navigation).toBe(300)
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
