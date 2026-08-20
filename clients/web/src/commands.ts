import type { HotkeySequence, RegisterableHotkey } from '@tanstack/react-hotkeys'
import type { AppDispatch, RootState } from './state'
import { actions } from './state'

export type CommandId =
  | 'palette.open'
  | 'help.open'
  | 'navigation.open'
  | 'surface.escape'
  | 'selection.next'
  | 'selection.previous'
  | 'selection.first'
  | 'selection.last'
  | 'layout.toggle'
  | 'density.toggle'
  | 'theme.toggle'
  | 'detail.full'
  | 'detail.condensed'
  | 'detail.results'

export interface CommandContext {
  dispatch: AppDispatch
  getState: () => RootState
  timelineIds: readonly string[]
  focusTimeline: () => void
}

export interface CommandBinding {
  label: string
  registration?:
    | { kind: 'hotkey'; hotkey: RegisterableHotkey }
    | { kind: 'sequence'; sequence: HotkeySequence }
}

export interface CommandDefinition {
  id: CommandId
  title: string
  description: string
  category: 'Navigate' | 'View' | 'Surface'
  bindings: readonly CommandBinding[]
  available: (context: CommandContext) => boolean
  run: (context: CommandContext) => void
}

const always = () => true
export const commandRegistry: readonly CommandDefinition[] = [
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
      const currentIndex = Math.max(context.timelineIds.indexOf(current ?? ''), 0)
      const nextIndex = Math.min(currentIndex + 1, context.timelineIds.length - 1)
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
    id: 'selection.first',
    title: 'Select first loaded item',
    description: 'Move to the earliest item in the loaded cursor window.',
    category: 'Navigate',
    bindings: [
      { label: 'g g', registration: { kind: 'sequence', sequence: ['G', 'G'] } },
      { label: 'Home' },
    ],
    available: (context) => context.timelineIds.length > 0,
    run: (context) => context.dispatch(actions.timelineSelected(context.timelineIds[0] ?? null)),
  },
  {
    id: 'selection.last',
    title: 'Select latest loaded item',
    description: 'Move to the latest item in the loaded cursor window.',
    category: 'Navigate',
    bindings: [
      { label: 'G', registration: { kind: 'hotkey', hotkey: 'Shift+G' } },
      { label: 'End' },
    ],
    available: (context) => context.timelineIds.length > 0,
    run: (context) =>
      context.dispatch(actions.timelineSelected(context.timelineIds.at(-1) ?? null)),
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
]

export const globalHotkeyBindings = commandRegistry.flatMap((command) =>
  command.bindings.flatMap((binding) =>
    binding.registration?.kind === 'hotkey'
      ? [{ commandId: command.id, hotkey: binding.registration.hotkey }]
      : [],
  ),
)

export const globalHotkeySequenceBindings = commandRegistry.flatMap((command) =>
  command.bindings.flatMap((binding) =>
    binding.registration?.kind === 'sequence'
      ? [{ commandId: command.id, sequence: binding.registration.sequence }]
      : [],
  ),
)

export const commandById = (id: CommandId): CommandDefinition => {
  const command = commandRegistry.find((candidate) => candidate.id === id)
  if (!command) throw new Error(`Unregistered command: ${id}`)
  return command
}

export const invokeCommand = (id: CommandId, context: CommandContext): void => {
  const command = commandById(id)
  if (command.available(context)) command.run(context)
}
