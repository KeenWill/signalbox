import { type CommandContext, type CommandId, commandRegistry, invokeCommand } from './commands'
import { actions } from './state'

export interface ProductCommandContext extends CommandContext {
  navigate: (path: string) => void
  navigateTimelineWindow: (anchor: 'first' | 'latest') => void
  openNavigation: () => void
  openPalette: () => void
  prepareFocusLayout: () => void
  timelineWindowAvailable: boolean
}

const navigateProductSurface = (context: ProductCommandContext, path: string) => {
  context.dispatch(actions.overlaySet(null))
  context.navigate(path)
}

const productNavigationCommands = [
  {
    id: 'palette.open',
    title: 'Open command palette',
    description: 'Browse every available application command.',
    category: 'Surface',
    bindings: [{ label: 'Mod+K', registration: { kind: 'hotkey', hotkey: 'Mod+K' } }],
    run: (context: ProductCommandContext) => context.openPalette(),
  },
  {
    id: 'layout.toggle',
    title: 'Toggle focus/workbench layout',
    description: 'Switch between a quiet transcript and the full operator workspace.',
    category: 'View',
    bindings: [{ label: 'Shift+W', registration: { kind: 'hotkey', hotkey: 'Shift+W' } }],
    run: (context: ProductCommandContext) => {
      context.prepareFocusLayout()
      const current = context.getState().app.layout
      context.dispatch(actions.layoutSet(current === 'focus' ? 'workbench' : 'focus'))
    },
  },
  {
    id: 'navigation.open',
    title: 'Open product navigation',
    description: 'Choose a Signalbox product surface.',
    category: 'Surface',
    bindings: [],
    run: (context: ProductCommandContext) => context.openNavigation(),
  },
  {
    id: 'navigate.attention',
    title: 'Go to Attention',
    description: 'Open the operator intervention queue.',
    category: 'Navigate',
    bindings: [{ label: 'g a', registration: { kind: 'sequence', sequence: ['G', 'A'] } }],
    run: (context: ProductCommandContext) => navigateProductSurface(context, '/attention'),
  },
  {
    id: 'navigate.sessions',
    title: 'Go to Sessions',
    description: 'Open the bounded session workspace.',
    category: 'Navigate',
    bindings: [{ label: 'g s', registration: { kind: 'sequence', sequence: ['G', 'S'] } }],
    run: (context: ProductCommandContext) => navigateProductSurface(context, '/sessions'),
  },
  {
    id: 'navigate.activity',
    title: 'Go to Activity',
    description: 'Open the system-wide event stream.',
    category: 'Navigate',
    bindings: [],
    run: (context: ProductCommandContext) => navigateProductSurface(context, '/activity'),
  },
  {
    id: 'navigate.imports',
    title: 'Go to Imports',
    description: 'Open conversation import operations.',
    category: 'Navigate',
    bindings: [],
    run: (context: ProductCommandContext) => navigateProductSurface(context, '/imports'),
  },
  {
    id: 'navigate.reviews',
    title: 'Go to Reviews',
    description: 'Open approval work and history.',
    category: 'Navigate',
    bindings: [],
    run: (context: ProductCommandContext) => navigateProductSurface(context, '/reviews'),
  },
  {
    id: 'navigate.runners',
    title: 'Go to Runners',
    description: 'Open runner capacity and health.',
    category: 'Navigate',
    bindings: [],
    run: (context: ProductCommandContext) => navigateProductSurface(context, '/runners'),
  },
  {
    id: 'navigate.search',
    title: 'Go to Search',
    description: 'Open cross-session search.',
    category: 'Navigate',
    bindings: [],
    run: (context: ProductCommandContext) => navigateProductSurface(context, '/search'),
  },
  {
    id: 'navigate.usage',
    title: 'Go to Usage',
    description: 'Open token and cost analysis.',
    category: 'Navigate',
    bindings: [],
    run: (context: ProductCommandContext) => navigateProductSurface(context, '/usage'),
  },
  {
    id: 'navigate.settings',
    title: 'Go to Settings',
    description: 'Open browser-local workstation preferences.',
    category: 'Navigate',
    bindings: [{ label: 'g ,', registration: { kind: 'sequence', sequence: ['G', ','] } }],
    run: (context: ProductCommandContext) => navigateProductSurface(context, '/settings'),
  },
] as const

const productTimelineCommands = [
  {
    id: 'selection.first',
    title: 'Open first session window',
    description: 'Load the server window containing the first session item.',
    category: 'Navigate',
    bindings: [{ label: 'g g', registration: { kind: 'sequence', sequence: ['G', 'G'] } }],
    available: (context: ProductCommandContext) => context.timelineWindowAvailable,
    run: (context: ProductCommandContext) => context.navigateTimelineWindow('first'),
  },
  {
    id: 'selection.last',
    title: 'Open latest session window',
    description: 'Load the server window containing the latest session item.',
    category: 'Navigate',
    bindings: [{ label: 'G', registration: { kind: 'hotkey', hotkey: 'Shift+G' } }],
    available: (context: ProductCommandContext) => context.timelineWindowAvailable,
    run: (context: ProductCommandContext) => context.navigateTimelineWindow('latest'),
  },
] as const

// Every id this module redefines. The scenario registry keeps its own definitions for the
// scenario studio; spreading both would list each product command twice and register its
// bindings twice.
const productOverriddenCommandIds: ReadonlySet<string> = new Set([
  ...productNavigationCommands.map((command) => command.id),
  ...productTimelineCommands.map((command) => command.id),
])

export const productCommandRegistry = [
  ...productNavigationCommands,
  ...productTimelineCommands,
  ...commandRegistry.filter((command) => !productOverriddenCommandIds.has(command.id)),
]
export type ProductCommandId = (typeof productCommandRegistry)[number]['id']

export const productHotkeyBindings = productCommandRegistry.flatMap((command) =>
  command.bindings.flatMap((binding) =>
    'registration' in binding && binding.registration.kind === 'hotkey'
      ? [{ commandId: command.id, hotkey: binding.registration.hotkey }]
      : [],
  ),
)

export const productHotkeySequenceBindings = productCommandRegistry.flatMap((command) =>
  command.bindings.flatMap((binding) =>
    'registration' in binding && binding.registration.kind === 'sequence'
      ? [{ commandId: command.id, sequence: [...binding.registration.sequence] }]
      : [],
  ),
)

export const invokeProductCommand = (
  id: ProductCommandId,
  context: ProductCommandContext,
): void => {
  const navigationCommand = productNavigationCommands.find((command) => command.id === id)
  if (navigationCommand) navigationCommand.run(context)
  else {
    const timelineCommand = productTimelineCommands.find((command) => command.id === id)
    if (timelineCommand) {
      if (timelineCommand.available(context)) timelineCommand.run(context)
    } else invokeCommand(id as CommandId, context)
  }
}
