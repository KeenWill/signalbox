import type { HotkeySequence, RegisterableHotkey } from '@tanstack/react-hotkeys'
import type { AppDispatch, RootState } from './state'
import { actions } from './state'

export interface CommandContext {
  dispatch: AppDispatch
  getState: () => RootState
  timelineIds: readonly string[]
  artifactPreviewIds: readonly string[]
  artifactOriginalIds: readonly string[]
  artifactSelectionTarget?: string
  focusTimeline: () => void
  importEntryIds?: readonly string[]
  selectedImportEntry?: string | null
  requestedImportEntry?: string
  selectImportEntry?: (id: string) => void
  openFirstTimelineWindow?: () => void
  openLatestTimelineWindow?: () => void
  onTimelineSelected?: (eventSequence: string) => void
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
  category: 'Navigate' | 'View' | 'Surface' | 'Imports' | 'Artifact'
  bindings: readonly CommandBinding[]
  available: (context: CommandContext) => boolean
  run: (context: CommandContext) => void
}

const always = () => true
const selectTimeline = (context: CommandContext, eventSequence: string | undefined): void => {
  const selected = eventSequence ?? null
  context.dispatch(actions.timelineSelected(selected))
  if (selected !== null) context.onTimelineSelected?.(selected)
}
const selectedArtifact = (context: CommandContext) => context.getState().app.selectedArtifact
const hasSelectedArtifactPreview = (context: CommandContext) => {
  const id = selectedArtifact(context)
  return id !== null && context.artifactPreviewIds.includes(id)
}
export const commandRegistry = [
  {
    id: 'artifact.select',
    title: 'Select artifact',
    description: 'Select the artifact targeted by the invoking control.',
    category: 'Artifact',
    bindings: [],
    available: (context) => context.artifactSelectionTarget !== undefined,
    run: (context) => {
      if (context.artifactSelectionTarget !== undefined) {
        context.dispatch(actions.artifactSelected(context.artifactSelectionTarget))
      }
    },
  },
  {
    id: 'artifact.preview.expand',
    title: 'Expand bounded artifact preview',
    description: 'Show the larger bounded projection of the selected artifact.',
    category: 'Artifact',
    bindings: [],
    available: (context) => {
      const id = selectedArtifact(context)
      return (
        id !== null &&
        hasSelectedArtifactPreview(context) &&
        !context.getState().app.expandedArtifacts[id]
      )
    },
    run: (context) => {
      const id = selectedArtifact(context)
      if (id !== null) context.dispatch(actions.artifactExpansionSet({ id, expanded: true }))
    },
  },
  {
    id: 'artifact.preview.collapse',
    title: 'Collapse artifact preview',
    description: 'Return the selected artifact to its initial bounded projection.',
    category: 'Artifact',
    bindings: [],
    available: (context) => {
      const id = selectedArtifact(context)
      return (
        id !== null &&
        hasSelectedArtifactPreview(context) &&
        Boolean(context.getState().app.expandedArtifacts[id])
      )
    },
    run: (context) => {
      const id = selectedArtifact(context)
      if (id !== null) context.dispatch(actions.artifactExpansionSet({ id, expanded: false }))
    },
  },
  {
    id: 'artifact.original.load',
    title: 'Load artifact original',
    description: 'Request the admitted browser-native original for the selected artifact.',
    category: 'Artifact',
    bindings: [],
    available: (context) => {
      const id = selectedArtifact(context)
      const originalState = id === null ? undefined : context.getState().app.originalArtifacts[id]
      return (
        id !== null &&
        context.artifactOriginalIds.includes(id) &&
        originalState !== 'loading' &&
        originalState !== 'loaded'
      )
    },
    run: (context) => {
      const id = selectedArtifact(context)
      if (id !== null) context.dispatch(actions.artifactOriginalRequested(id))
    },
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
      selectTimeline(context, context.timelineIds[nextIndex])
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
      selectTimeline(context, context.timelineIds[previousIndex])
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
    available: (context) =>
      context.openFirstTimelineWindow !== undefined || context.timelineIds.length > 0,
    run: (context) => {
      if (context.openFirstTimelineWindow) context.openFirstTimelineWindow()
      else selectTimeline(context, context.timelineIds[0])
    },
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
    available: (context) =>
      context.openLatestTimelineWindow !== undefined || context.timelineIds.length > 0,
    run: (context) => {
      if (context.openLatestTimelineWindow) context.openLatestTimelineWindow()
      else selectTimeline(context, context.timelineIds.at(-1))
    },
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
