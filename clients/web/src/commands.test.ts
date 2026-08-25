import { describe, expect, it } from 'vitest'
import { commandById, invokeCommand } from './commands'
import { productCommandRegistry } from './productCommands'
import { actions, selectApp, store } from './state'

describe('command registry', () => {
  it('keeps keyboard help available without timeline rows', () => {
    expect(
      commandById('help.open').available({
        dispatch: store.dispatch,
        getState: store.getState,
        timelineIds: [],
        focusTimeline: () => undefined,
      }),
    ).toBe(true)
  })

  it('omits scenario navigation from the product command registry', () => {
    const productCommandIds: readonly string[] = productCommandRegistry.map((command) => command.id)
    expect(productCommandIds).not.toContain('navigation.open')
    expect(new Set(productCommandIds).size).toBe(productCommandIds.length)
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

  it('routes catalog actions and rapid switching through registered commands', () => {
    const calls: string[] = []
    const context = {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
      sessionCatalogAvailable: true,
      sessionWorkspaceAvailable: true,
      applySessionSearch: () => calls.push('search'),
      loadMoreSessions: () => calls.push('more'),
      loadMoreSessionsAvailable: true,
      toggleSessionSort: () => calls.push('sort'),
      selectSession: (offset: -1 | 1) => calls.push(`select:${offset}`),
      switchSession: (offset: -1 | 1) => calls.push(`switch:${offset}`),
      openSelectedSession: () => calls.push('open'),
    }

    invokeCommand('session.catalog.apply-search', context)
    invokeCommand('session.catalog.sort', context)
    invokeCommand('session.catalog.more', context)
    invokeCommand('session.catalog.next', context)
    invokeCommand('session.catalog.open', context)
    invokeCommand('session.switch.previous', context)

    expect(calls).toEqual(['search', 'sort', 'more', 'select:1', 'open', 'switch:-1'])
  })

  it('routes settings choices through registered commands', () => {
    const context = {
      dispatch: store.dispatch,
      getState: store.getState,
      timelineIds: [],
      focusTimeline: () => undefined,
    }

    invokeCommand('layout.focus', context)
    invokeCommand('density.compact', context)
    invokeCommand('detail.results', context)
    invokeCommand('theme.light', context)
    invokeCommand('preferences.panes.set', {
      ...context,
      preferencePaneSizes: { navigation: 240, inspector: 360 },
    })

    expect(selectApp(store.getState())).toMatchObject({
      layout: 'focus',
      density: 'compact',
      detail: 'results',
      theme: 'light',
      paneSizes: { navigation: 240, inspector: 360 },
    })
  })
})
