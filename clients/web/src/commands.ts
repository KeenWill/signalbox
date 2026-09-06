import type { HotkeySequence, RegisterableHotkey } from '@tanstack/react-hotkeys'
import type { BrowserPreferences } from './preferences'
import type { AppDispatch, DensityMode, LayoutMode, RootState, ThemeMode } from './state'
import { actions } from './state'

export interface CommandContext {
  dispatch: AppDispatch
  getState: () => RootState
  timelineIds: readonly string[]
  artifactPreviewIds: readonly string[]
  artifactOriginalIds: readonly string[]
  artifactSelectionTarget?: string
  paneSize?: number
  sessionId?: string
  timelineWindowAvailable?: boolean
  focusTimeline: () => void
  searchAvailable?: boolean
  focusSearch?: () => void
  importEntryIds?: readonly string[]
  selectedImportEntry?: string | null
  requestedImportEntry?: string
  selectImportEntry?: (id: string) => void
  canSelectImportEntry?: boolean
  canContinueImport?: boolean
  continueImport?: (relationship: 'resume' | 'fork') => void
  canRetryImport?: boolean
  retryImport?: () => void
  canAbandonImport?: boolean
  abandonImport?: () => void
  loadTimelineWindow?: (anchor: 'first' | 'latest') => void
  navigate?: (path: string) => void
  configuresTranscriptDetail?: boolean
  openArtifactInspector?: () => void
  openSession?: (sessionId: string) => void
  toggleTimelineExpansion?: () => void
  unwindSurface?: () => boolean
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
  category: 'Navigate' | 'View' | 'Surface' | 'Artifact' | 'Settings' | 'Imports'
  bindings: readonly CommandBinding[]
  available: (context: CommandContext) => boolean
  run: (context: CommandContext) => void
}

const always = () => true
const selectedArtifact = (context: CommandContext) => context.getState().app.selectedArtifact
const hasSelectedArtifactPreview = (context: CommandContext) => {
  const id = selectedArtifact(context)
  return id !== null && context.artifactPreviewIds.includes(id)
}
const productNavigation = (context: CommandContext) => context.navigate !== undefined
const transcriptDetail = (context: CommandContext) =>
  context.timelineIds.length > 0 || context.configuresTranscriptDetail === true
const paneSizeProvided = (context: CommandContext) => context.paneSize !== undefined
const setLayout = (layout: LayoutMode) => (context: CommandContext) =>
  context.dispatch(actions.layoutSet(layout))
const setDensity = (density: DensityMode) => (context: CommandContext) =>
  context.dispatch(actions.densitySet(density))
const setTheme = (theme: ThemeMode) => (context: CommandContext) =>
  context.dispatch(actions.themeSet(theme))
const artifactInspector = (context: CommandContext) => context.openArtifactInspector !== undefined
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
    id: 'navigate.scenario',
    title: 'Go to Scenario Studio',
    description: 'Open the streaming interaction scenario.',
    category: 'Navigate',
    bindings: [],
    available: productNavigation,
    run: (context) => context.navigate?.('/scenario/streaming'),
  },
  {
    id: 'artifact.open',
    title: 'Open artifact inspector',
    description: 'Resolve and inspect an immutable blob by its server-provided identity.',
    category: 'Surface',
    bindings: [],
    available: artifactInspector,
    run: (context) => context.openArtifactInspector?.(),
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
    id: 'search.focus',
    title: 'Focus lexical search',
    description: 'Move directly to the bounded canonical-evidence search field.',
    category: 'Navigate',
    bindings: [{ label: 'Mod+Shift+F', registration: { kind: 'hotkey', hotkey: 'Mod+Shift+F' } }],
    available: (context) => context.searchAvailable === true,
    run: (context) => context.focusSearch?.(),
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
      else if (context.unwindSurface?.()) return
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
    id: 'imports.entry.select',
    title: 'Select imported frontier',
    description: 'Select the requested immutable imported entry.',
    category: 'Imports',
    bindings: [],
    available: (context) =>
      context.canSelectImportEntry === true &&
      context.requestedImportEntry !== undefined &&
      context.selectImportEntry !== undefined,
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
      context.canSelectImportEntry === true &&
      (context.importEntryIds?.length ?? 0) > 0 &&
      context.selectImportEntry !== undefined,
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
      context.canSelectImportEntry === true &&
      (context.importEntryIds?.length ?? 0) > 0 &&
      context.selectImportEntry !== undefined,
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
      context.canSelectImportEntry === true &&
      (context.importEntryIds?.length ?? 0) > 0 &&
      context.selectImportEntry !== undefined,
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
      context.canSelectImportEntry === true &&
      (context.importEntryIds?.length ?? 0) > 0 &&
      context.selectImportEntry !== undefined,
    run: (context) => context.selectImportEntry?.(context.importEntryIds?.at(-1) ?? ''),
  },
  {
    id: 'imports.continue.resume',
    title: 'Resume from imported frontier',
    description: 'Create a native session by resuming the selected imported frontier.',
    category: 'Imports',
    bindings: [],
    available: (context) =>
      context.canContinueImport === true && context.continueImport !== undefined,
    run: (context) => context.continueImport?.('resume'),
  },
  {
    id: 'imports.continue.fork',
    title: 'Fork from imported frontier',
    description: 'Create a native session by forking the selected imported frontier.',
    category: 'Imports',
    bindings: [],
    available: (context) =>
      context.canContinueImport === true && context.continueImport !== undefined,
    run: (context) => context.continueImport?.('fork'),
  },
  {
    id: 'imports.continue.retry',
    title: 'Retry exact imported continuation',
    description: 'Replay the retained imported-continuation command without changing its payload.',
    category: 'Imports',
    bindings: [],
    available: (context) => context.canRetryImport === true && context.retryImport !== undefined,
    run: (context) => context.retryImport?.(),
  },
  {
    id: 'imports.continue.abandon',
    title: 'Abandon exact imported continuation',
    description: 'Discard the retained imported-continuation command after explicit confirmation.',
    category: 'Imports',
    bindings: [],
    available: (context) =>
      context.canAbandonImport === true && context.abandonImport !== undefined,
    run: (context) => context.abandonImport?.(),
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
      if (current !== 'focus') context.focusTimeline()
      context.dispatch(actions.layoutSet(current === 'focus' ? 'workbench' : 'focus'))
    },
  },
  {
    id: 'layout.workbench',
    title: 'Use workbench layout',
    description: 'Show navigation, the primary surface, and the contextual inspector.',
    category: 'Settings',
    bindings: [],
    available: always,
    run: setLayout('workbench'),
  },
  {
    id: 'layout.focus',
    title: 'Use focus layout',
    description: 'Show the primary surface without secondary panes.',
    category: 'Settings',
    bindings: [],
    available: always,
    run: setLayout('focus'),
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
    description: 'Use dense rows for high-volume operator work.',
    category: 'Settings',
    bindings: [],
    available: always,
    run: setDensity('compact'),
  },
  {
    id: 'density.comfortable',
    title: 'Use comfortable density',
    description: 'Add separation without changing information detail.',
    category: 'Settings',
    bindings: [],
    available: always,
    run: setDensity('comfortable'),
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
    category: 'Settings',
    bindings: [],
    available: always,
    run: setTheme('dark'),
  },
  {
    id: 'theme.light',
    title: 'Use light theme',
    description: 'Use the light workstation color theme.',
    category: 'Settings',
    bindings: [],
    available: always,
    run: setTheme('light'),
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
    id: 'pane.navigation.preview',
    title: 'Preview navigation pane size',
    description: 'Preview the browser-local navigation pane width without persisting it.',
    category: 'Settings',
    bindings: [],
    available: paneSizeProvided,
    run: (context) => {
      if (context.paneSize === undefined) return
      const paneSizes: BrowserPreferences['paneSizes'] = {
        ...context.getState().app.paneSizes,
        navigation: context.paneSize,
      }
      context.dispatch(actions.paneSizesPreviewed(paneSizes))
    },
  },
  {
    id: 'pane.navigation.resize',
    title: 'Resize navigation pane',
    description: 'Set the browser-local navigation pane width.',
    category: 'Settings',
    bindings: [],
    available: paneSizeProvided,
    run: (context) => {
      if (context.paneSize === undefined) return
      const paneSizes: BrowserPreferences['paneSizes'] = {
        ...context.getState().app.paneSizes,
        navigation: context.paneSize,
      }
      context.dispatch(actions.paneSizesSet(paneSizes))
    },
  },
  {
    id: 'pane.inspector.preview',
    title: 'Preview inspector pane size',
    description: 'Preview the browser-local inspector pane width without persisting it.',
    category: 'Settings',
    bindings: [],
    available: paneSizeProvided,
    run: (context) => {
      if (context.paneSize === undefined) return
      const paneSizes: BrowserPreferences['paneSizes'] = {
        ...context.getState().app.paneSizes,
        inspector: context.paneSize,
      }
      context.dispatch(actions.paneSizesPreviewed(paneSizes))
    },
  },
  {
    id: 'pane.inspector.resize',
    title: 'Resize inspector pane',
    description: 'Set the browser-local inspector pane width.',
    category: 'Settings',
    bindings: [],
    available: paneSizeProvided,
    run: (context) => {
      if (context.paneSize === undefined) return
      const paneSizes: BrowserPreferences['paneSizes'] = {
        ...context.getState().app.paneSizes,
        inspector: context.paneSize,
      }
      context.dispatch(actions.paneSizesSet(paneSizes))
    },
  },
  {
    id: 'preferences.reset',
    title: 'Restore preference defaults',
    description: 'Restore every browser-local workstation preference to its default.',
    category: 'Settings',
    bindings: [],
    available: always,
    run: (context) => context.dispatch(actions.preferencesReset()),
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
