import type { HotkeySequence, RegisterableHotkey } from '@tanstack/react-hotkeys'
import type { AppDispatch, RootState } from './state'
import { actions } from './state'

export interface CommandContext {
  dispatch: AppDispatch
  getState: () => RootState
  timelineIds: readonly string[]
  paneSize?: number
  sessionId?: string
  timelineWindowAvailable?: boolean
  focusTimeline: () => void
  loadTimelineWindow?: (anchor: 'first' | 'latest') => void
  navigate?: (path: string) => void
  openSession?: (sessionId: string) => void
  toggleTimelineExpansion?: () => void
}

export interface CommandBinding {
  label: string
  registration?:
    | { kind: 'hotkey'; hotkey: RegisterableHotkey }
    | { kind: 'sequence'; sequence: HotkeySequence }
}

interface CommandDefinitionShape {
  id: string
  title: string
  description: string
  category: 'Navigate' | 'View' | 'Surface'
  bindings: readonly CommandBinding[]
  available: (context: CommandContext) => boolean
  run: (context: CommandContext) => void
}

const always = () => true
const productNavigation = (context: CommandContext) => context.navigate !== undefined
export const commandRegistry = [
  {
    id: 'navigate.attention',
    title: 'Go to Attention',
    description: 'Open the operator intervention queue.',
    category: 'Navigate',
    bindings: [{ label: 'g a', registration: { kind: 'sequence', sequence: ['G', 'A'] } }],
    available: productNavigation,
    run: (context) => context.navigate?.('/attention'),
  },
  {
    id: 'navigate.sessions',
    title: 'Go to Sessions',
    description: 'Open the bounded session workspace.',
    category: 'Navigate',
    bindings: [{ label: 'g s', registration: { kind: 'sequence', sequence: ['G', 'S'] } }],
    available: productNavigation,
    run: (context) => context.navigate?.('/sessions'),
  },
  {
    id: 'navigate.activity',
    title: 'Go to Activity',
    description: 'Open the system-wide event stream.',
    category: 'Navigate',
    bindings: [],
    available: productNavigation,
    run: (context) => context.navigate?.('/activity'),
  },
  {
    id: 'navigate.imports',
    title: 'Go to Imports',
    description: 'Open conversation import operations.',
    category: 'Navigate',
    bindings: [],
    available: productNavigation,
    run: (context) => context.navigate?.('/imports'),
  },
  {
    id: 'navigate.reviews',
    title: 'Go to Reviews',
    description: 'Open approval work and history.',
    category: 'Navigate',
    bindings: [],
    available: productNavigation,
    run: (context) => context.navigate?.('/reviews'),
  },
  {
    id: 'navigate.runners',
    title: 'Go to Runners',
    description: 'Open runner capacity and health.',
    category: 'Navigate',
    bindings: [],
    available: productNavigation,
    run: (context) => context.navigate?.('/runners'),
  },
  {
    id: 'navigate.search',
    title: 'Go to Search',
    description: 'Open cross-session search.',
    category: 'Navigate',
    bindings: [],
    available: productNavigation,
    run: (context) => context.navigate?.('/search'),
  },
  {
    id: 'navigate.usage',
    title: 'Go to Usage',
    description: 'Open token and cost analysis.',
    category: 'Navigate',
    bindings: [],
    available: productNavigation,
    run: (context) => context.navigate?.('/usage'),
  },
  {
    id: 'navigate.settings',
    title: 'Go to Settings',
    description: 'Open browser-local workstation preferences.',
    category: 'Navigate',
    bindings: [{ label: 'g ,', registration: { kind: 'sequence', sequence: ['G', ','] } }],
    available: productNavigation,
    run: (context) => context.navigate?.('/settings'),
  },
  {
    id: 'palette.open',
    title: 'Open command palette',
    description: 'Browse every available application command.',
    category: 'Surface',
    bindings: [{ label: 'Mod+K', registration: { kind: 'hotkey', hotkey: 'Mod+K' } }],
    available: always,
    run: (context) => context.dispatch(actions.overlaySet('palette')),
  },
  {
    id: 'help.open',
    title: 'Open keyboard help',
    description: 'Review modal navigation and command bindings.',
    category: 'Surface',
    bindings: [{ label: '?', registration: { kind: 'hotkey', hotkey: { key: '/', shift: true } } }],
    available: always,
    run: (context) => context.dispatch(actions.overlaySet('help')),
  },
  {
    id: 'navigation.open',
    title: 'Open scenario navigation',
    description: 'Choose a deterministic development scenario.',
    category: 'Surface',
    bindings: [],
    available: always,
    run: (context) => context.dispatch(actions.overlaySet('navigation')),
  },
  {
    id: 'surface.escape',
    title: 'Unwind current surface',
    description: 'Close the nearest overlay or leave editing and return to the timeline.',
    category: 'Surface',
    bindings: [{ label: 'Escape', registration: { kind: 'hotkey', hotkey: 'Escape' } }],
    available: always,
    run: (context) => {
      if (context.getState().app.overlay !== null) context.dispatch(actions.overlaySet(null))
      else context.focusTimeline()
    },
  },
  {
    id: 'selection.next',
    title: 'Select next timeline item',
    description: 'Move the timeline selection toward the latest item.',
    category: 'Navigate',
    bindings: [
      { label: 'j', registration: { kind: 'hotkey', hotkey: 'J' } },
      { label: 'ArrowDown' },
    ],
    available: (context) => context.timelineIds.length > 0,
    run: (context) => {
      const current = context.getState().app.selectedTimeline
      const currentIndex = context.timelineIds.indexOf(current ?? '')
      const nextIndex =
        currentIndex < 0 ? 0 : Math.min(currentIndex + 1, context.timelineIds.length - 1)
      context.dispatch(actions.timelineSelected(context.timelineIds[nextIndex] ?? null))
    },
  },
  {
    id: 'selection.previous',
    title: 'Select previous timeline item',
    description: 'Move the timeline selection toward the first item.',
    category: 'Navigate',
    bindings: [{ label: 'k', registration: { kind: 'hotkey', hotkey: 'K' } }, { label: 'ArrowUp' }],
    available: (context) => context.timelineIds.length > 0,
    run: (context) => {
      const current = context.getState().app.selectedTimeline
      const currentIndex = Math.max(context.timelineIds.indexOf(current ?? ''), 0)
      const previousIndex = Math.max(currentIndex - 1, 0)
      context.dispatch(actions.timelineSelected(context.timelineIds[previousIndex] ?? null))
    },
  },
  {
    id: 'selection.toggleExpansion',
    title: 'Toggle selected timeline item detail',
    description: 'Expand or collapse the selected timeline item.',
    category: 'View',
    bindings: [{ label: 'Enter / Space' }],
    available: (context) =>
      context.getState().app.selectedTimeline !== null &&
      context.timelineIds.includes(context.getState().app.selectedTimeline ?? '') &&
      context.toggleTimelineExpansion !== undefined,
    run: (context) => context.toggleTimelineExpansion?.(),
  },
  {
    id: 'selection.first',
    title: 'Go to first timeline item',
    description: 'Load the first timeline window or select its first loaded item.',
    category: 'Navigate',
    bindings: [
      { label: 'g g', registration: { kind: 'sequence', sequence: ['G', 'G'] } },
      { label: 'Home' },
    ],
    available: (context) =>
      context.timelineIds.length > 0 || context.timelineWindowAvailable === true,
    run: (context) => {
      if (context.loadTimelineWindow) context.loadTimelineWindow('first')
      else context.dispatch(actions.timelineSelected(context.timelineIds[0] ?? null))
    },
  },
  {
    id: 'selection.last',
    title: 'Go to latest timeline item',
    description: 'Load the latest timeline window or select its latest loaded item.',
    category: 'Navigate',
    bindings: [
      { label: 'G', registration: { kind: 'hotkey', hotkey: 'Shift+G' } },
      { label: 'End' },
    ],
    available: (context) =>
      context.timelineIds.length > 0 || context.timelineWindowAvailable === true,
    run: (context) => {
      if (context.loadTimelineWindow) context.loadTimelineWindow('latest')
      else context.dispatch(actions.timelineSelected(context.timelineIds.at(-1) ?? null))
    },
  },
  {
    id: 'layout.toggle',
    title: 'Toggle focus/workbench layout',
    description: 'Switch between a quiet transcript and the full operator workspace.',
    category: 'View',
    bindings: [{ label: 'Shift+W', registration: { kind: 'hotkey', hotkey: 'Shift+W' } }],
    available: always,
    run: (context) => {
      const current = context.getState().app.layout
      if (current === 'workbench') context.focusTimeline()
      context.dispatch(actions.layoutSet(current === 'focus' ? 'workbench' : 'focus'))
    },
  },
  {
    id: 'density.toggle',
    title: 'Toggle visual density',
    description: 'Switch compact and comfortable spacing independently of detail.',
    category: 'View',
    bindings: [{ label: 'Shift+D', registration: { kind: 'hotkey', hotkey: 'Shift+D' } }],
    available: always,
    run: (context) => {
      const current = context.getState().app.density
      context.dispatch(actions.densitySet(current === 'compact' ? 'comfortable' : 'compact'))
    },
  },
  {
    id: 'theme.toggle',
    title: 'Toggle light/dark theme',
    description: 'Switch the CSS-variable theme.',
    category: 'View',
    bindings: [{ label: 'Shift+T', registration: { kind: 'hotkey', hotkey: 'Shift+T' } }],
    available: always,
    run: (context) => {
      const current = context.getState().app.theme
      context.dispatch(actions.themeSet(current === 'dark' ? 'light' : 'dark'))
    },
  },
  {
    id: 'detail.full',
    title: 'Show full transcript detail',
    description: 'Show every supported timeline record.',
    category: 'View',
    bindings: [],
    available: always,
    run: (context) => context.dispatch(actions.detailSet('full')),
  },
  {
    id: 'detail.condensed',
    title: 'Show condensed transcript detail',
    description: 'Keep origins, tools, progress, warnings, and results compact.',
    category: 'View',
    bindings: [],
    available: always,
    run: (context) => context.dispatch(actions.detailSet('condensed')),
  },
  {
    id: 'detail.results',
    title: 'Show transcript results',
    description: 'Emphasize origins and durable results.',
    category: 'View',
    bindings: [],
    available: always,
    run: (context) => context.dispatch(actions.detailSet('results')),
  },
  {
    id: 'preferences.reset',
    title: 'Restore preference defaults',
    description: 'Restore browser-local workstation preferences to their defaults.',
    category: 'View',
    bindings: [],
    available: always,
    run: (context) => context.dispatch(actions.preferencesReset()),
  },
  {
    id: 'session.open',
    title: 'Open session workspace',
    description: 'Open a bounded workspace for an exact session identity.',
    category: 'Navigate',
    bindings: [],
    available: (context) => context.sessionId !== undefined && context.openSession !== undefined,
    run: (context) => {
      if (context.sessionId === undefined) return
      context.openSession?.(context.sessionId)
    },
  },
  {
    id: 'pane.navigation.resize',
    title: 'Resize navigation pane',
    description: 'Set the browser-local Workbench navigation pane width.',
    category: 'View',
    bindings: [],
    available: (context) => context.paneSize !== undefined,
    run: (context) => {
      if (context.paneSize === undefined) return
      context.dispatch(
        actions.paneSizesSet({
          ...context.getState().app.paneSizes,
          navigation: context.paneSize,
        }),
      )
    },
  },
  {
    id: 'pane.inspector.resize',
    title: 'Resize inspector pane',
    description: 'Set the browser-local Workbench inspector pane width.',
    category: 'View',
    bindings: [],
    available: (context) => context.paneSize !== undefined,
    run: (context) => {
      if (context.paneSize === undefined) return
      context.dispatch(
        actions.paneSizesSet({
          ...context.getState().app.paneSizes,
          inspector: context.paneSize,
        }),
      )
    },
  },
] as const satisfies readonly CommandDefinitionShape[]

export type CommandDefinition = (typeof commandRegistry)[number]
export type CommandId = CommandDefinition['id']

export const globalHotkeyBindings = commandRegistry.flatMap((command) => {
  const bindings: readonly CommandBinding[] = command.bindings
  return bindings.flatMap((binding) =>
    binding.registration?.kind === 'hotkey'
      ? [{ commandId: command.id, hotkey: binding.registration.hotkey }]
      : [],
  )
})

export const globalHotkeySequenceBindings = commandRegistry.flatMap((command) => {
  const bindings: readonly CommandBinding[] = command.bindings
  return bindings.flatMap((binding) =>
    binding.registration?.kind === 'sequence'
      ? [{ commandId: command.id, sequence: binding.registration.sequence }]
      : [],
  )
})

export const commandById = (id: CommandId): CommandDefinition => {
  const command = commandRegistry.find((candidate) => candidate.id === id)
  if (!command) throw new Error(`Unregistered command: ${id}`)
  return command
}

export const invokeCommand = (id: CommandId, context: CommandContext): void => {
  const command = commandById(id)
  if (command.available(context)) command.run(context)
}
