import type { HotkeySequence, RegisterableHotkey } from '@tanstack/react-hotkeys'
import type { AppDispatch, RootState } from './state'
import { actions } from './state'

export interface CommandContext {
  dispatch: AppDispatch
  getState: () => RootState
  timelineIds: readonly string[]
  focusTimeline: () => void
  importEntryIds?: readonly string[]
  selectedImportEntry?: string | null
  requestedImportEntry?: string
  selectImportEntry?: (id: string) => void
  navigate?: (path: string) => void
  transcriptPreferences?: boolean
  presentationPreferences?: boolean
}

export interface CommandBinding {
  label: string
  scope?: 'workspace' | 'imports'
  registration?:
    | { kind: 'hotkey'; hotkey: RegisterableHotkey }
    | { kind: 'sequence'; sequence: HotkeySequence }
}

interface CommandDefinitionShape {
  id: string
  title: string
  description: string
  category: 'Navigate' | 'View' | 'Surface' | 'Imports'
  bindings: readonly CommandBinding[]
  available: (context: CommandContext) => boolean
  run: (context: CommandContext) => void
}

const always = () => true
const productNavigation = (context: CommandContext) => context.navigate !== undefined
const scenarioTimeline = (context: CommandContext) => context.timelineIds.length > 0
const transcriptDetail = (context: CommandContext) =>
  scenarioTimeline(context) || context.transcriptPreferences === true
const presentationPreferences = (context: CommandContext) =>
  context.presentationPreferences === true
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
    description: 'Open the bounded session index.',
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
    available: scenarioTimeline,
    run: (context) => context.dispatch(actions.overlaySet('help')),
  },
  {
    id: 'navigation.open',
    title: 'Open navigation',
    description: 'Open navigation for the current application surface.',
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
    id: 'imports.entry.select',
    title: 'Select imported frontier',
    description: 'Select the requested immutable imported entry.',
    category: 'Imports',
    bindings: [],
    available: (context) =>
      context.requestedImportEntry !== undefined && context.selectImportEntry !== undefined,
    run: (context) => context.selectImportEntry?.(context.requestedImportEntry ?? ''),
  },
  {
    id: 'imports.entry.next',
    title: 'Select next imported frontier',
    description: 'Move toward the latest entry in the loaded import window.',
    category: 'Imports',
    bindings: [
      { label: 'j', scope: 'imports', registration: { kind: 'hotkey', hotkey: 'J' } },
      { label: 'ArrowDown', scope: 'imports' },
    ],
    available: (context) =>
      (context.importEntryIds?.length ?? 0) > 0 && context.selectImportEntry !== undefined,
    run: (context) => {
      const ids = context.importEntryIds ?? []
      const currentIndex = ids.indexOf(context.selectedImportEntry ?? '')
      const nextIndex = currentIndex < 0 ? 0 : Math.min(currentIndex + 1, ids.length - 1)
      context.selectImportEntry?.(ids[nextIndex] ?? '')
    },
  },
  {
    id: 'imports.entry.previous',
    title: 'Select previous imported frontier',
    description: 'Move toward the first entry in the loaded import window.',
    category: 'Imports',
    bindings: [
      { label: 'k', scope: 'imports', registration: { kind: 'hotkey', hotkey: 'K' } },
      { label: 'ArrowUp', scope: 'imports' },
    ],
    available: (context) =>
      (context.importEntryIds?.length ?? 0) > 0 && context.selectImportEntry !== undefined,
    run: (context) => {
      const ids = context.importEntryIds ?? []
      const currentIndex = Math.max(ids.indexOf(context.selectedImportEntry ?? ''), 0)
      context.selectImportEntry?.(ids[Math.max(currentIndex - 1, 0)] ?? '')
    },
  },
  {
    id: 'imports.entry.first',
    title: 'Select first loaded imported frontier',
    description: 'Move to the earliest entry in the loaded import window.',
    category: 'Imports',
    bindings: [
      {
        label: 'g g',
        scope: 'imports',
        registration: { kind: 'sequence', sequence: ['G', 'G'] },
      },
      { label: 'Home', scope: 'imports' },
    ],
    available: (context) =>
      (context.importEntryIds?.length ?? 0) > 0 && context.selectImportEntry !== undefined,
    run: (context) => context.selectImportEntry?.(context.importEntryIds?.[0] ?? ''),
  },
  {
    id: 'imports.entry.last',
    title: 'Select latest loaded imported frontier',
    description: 'Move to the latest entry in the loaded import window.',
    category: 'Imports',
    bindings: [
      {
        label: 'G',
        scope: 'imports',
        registration: { kind: 'hotkey', hotkey: 'Shift+G' },
      },
      { label: 'End', scope: 'imports' },
    ],
    available: (context) =>
      (context.importEntryIds?.length ?? 0) > 0 && context.selectImportEntry !== undefined,
    run: (context) => context.selectImportEntry?.(context.importEntryIds?.at(-1) ?? ''),
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
    id: 'layout.workbench',
    title: 'Use workbench layout',
    description: 'Show navigation, the primary surface, and the contextual inspector.',
    category: 'View',
    bindings: [],
    available: presentationPreferences,
    run: (context) => context.dispatch(actions.layoutSet('workbench')),
  },
  {
    id: 'layout.focus',
    title: 'Use focus layout',
    description: 'Show a quiet primary surface without secondary panes.',
    category: 'View',
    bindings: [],
    available: presentationPreferences,
    run: (context) => context.dispatch(actions.layoutSet('focus')),
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
    id: 'density.compact',
    title: 'Use compact density',
    description: 'Use dense spacing for high-volume operator work.',
    category: 'View',
    bindings: [],
    available: presentationPreferences,
    run: (context) => context.dispatch(actions.densitySet('compact')),
  },
  {
    id: 'density.comfortable',
    title: 'Use comfortable density',
    description: 'Use more separation without changing information detail.',
    category: 'View',
    bindings: [],
    available: presentationPreferences,
    run: (context) => context.dispatch(actions.densitySet('comfortable')),
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
    id: 'theme.dark',
    title: 'Use dark theme',
    description: 'Use the dark workstation color theme.',
    category: 'View',
    bindings: [],
    available: presentationPreferences,
    run: (context) => context.dispatch(actions.themeSet('dark')),
  },
  {
    id: 'theme.light',
    title: 'Use light theme',
    description: 'Use the light workstation color theme.',
    category: 'View',
    bindings: [],
    available: presentationPreferences,
    run: (context) => context.dispatch(actions.themeSet('light')),
  },
  {
    id: 'detail.full',
    title: 'Show full transcript detail',
    description: 'Show every supported timeline record.',
    category: 'View',
    bindings: [],
    available: transcriptDetail,
    run: (context) => context.dispatch(actions.detailSet('full')),
  },
  {
    id: 'detail.condensed',
    title: 'Show condensed transcript detail',
    description: 'Keep origins, tools, progress, warnings, and results compact.',
    category: 'View',
    bindings: [],
    available: transcriptDetail,
    run: (context) => context.dispatch(actions.detailSet('condensed')),
  },
  {
    id: 'detail.results',
    title: 'Show transcript results',
    description: 'Emphasize origins and durable results.',
    category: 'View',
    bindings: [],
    available: transcriptDetail,
    run: (context) => context.dispatch(actions.detailSet('results')),
  },
] as const satisfies readonly CommandDefinitionShape[]

export type CommandDefinition = (typeof commandRegistry)[number]
export type CommandId = CommandDefinition['id']

export const globalHotkeyBindings = commandRegistry.flatMap((command) => {
  const bindings: readonly CommandBinding[] = command.bindings
  return bindings.flatMap((binding) =>
    binding.scope !== 'imports' && binding.registration?.kind === 'hotkey'
      ? [{ commandId: command.id, hotkey: binding.registration.hotkey }]
      : [],
  )
})

export const globalHotkeySequenceBindings = commandRegistry.flatMap((command) => {
  const bindings: readonly CommandBinding[] = command.bindings
  return bindings.flatMap((binding) =>
    binding.scope !== 'imports' && binding.registration?.kind === 'sequence'
      ? [{ commandId: command.id, sequence: binding.registration.sequence }]
      : [],
  )
})

export const importHotkeyBindings = commandRegistry.flatMap((command) => {
  const bindings: readonly CommandBinding[] = command.bindings
  return bindings.flatMap((binding) =>
    binding.scope === 'imports' && binding.registration?.kind === 'hotkey'
      ? [{ commandId: command.id, hotkey: binding.registration.hotkey }]
      : [],
  )
})

export const importHotkeySequenceBindings = commandRegistry.flatMap((command) => {
  const bindings: readonly CommandBinding[] = command.bindings
  return bindings.flatMap((binding) =>
    binding.scope === 'imports' && binding.registration?.kind === 'sequence'
      ? [{ commandId: command.id, sequence: binding.registration.sequence }]
      : [],
  )
})

export const surfaceHotkeyBindings = commandRegistry.flatMap((command) => {
  const bindings: readonly CommandBinding[] = command.bindings
  return bindings.flatMap((binding) =>
    command.category === 'Surface' && binding.registration?.kind === 'hotkey'
      ? [{ commandId: command.id, hotkey: binding.registration.hotkey }]
      : [],
  )
})

export const surfaceHotkeySequenceBindings = commandRegistry.flatMap((command) => {
  const bindings: readonly CommandBinding[] = command.bindings
  return bindings.flatMap((binding) =>
    command.category === 'Surface' && binding.registration?.kind === 'sequence'
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
