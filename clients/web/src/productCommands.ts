import { type CommandContext, type CommandId, commandRegistry, invokeCommand } from './commands'
import { actions } from './state'

export interface ProductCommandContext extends CommandContext {
  navigate: (path: string) => void
  navigateTimelineWindow: (anchor: 'first' | 'latest') => void
}

const navigateProductSurface = (context: ProductCommandContext, path: string) => {
  context.dispatch(actions.overlaySet(null))
  context.navigate(path)
}

const productNavigationCommands = [
  {
    id: 'navigation.open',
    title: 'Open product navigation',
    description: 'Choose a Signalbox product surface.',
    category: 'Surface',
    bindings: [],
    run: (context: ProductCommandContext) => context.dispatch(actions.overlaySet('navigation')),
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
    available: (context: ProductCommandContext) => context.timelineIds.length > 0,
    run: (context: ProductCommandContext) => context.navigateTimelineWindow('first'),
  },
  {
    id: 'selection.last',
    title: 'Open latest session window',
    description: 'Load the server window containing the latest session item.',
    category: 'Navigate',
    bindings: [{ label: 'G', registration: { kind: 'hotkey', hotkey: 'Shift+G' } }],
    available: (context: ProductCommandContext) => context.timelineIds.length > 0,
    run: (context: ProductCommandContext) => context.navigateTimelineWindow('latest'),
  },
] as const

export const productCommandRegistry = [
  ...productNavigationCommands,
  ...productTimelineCommands,
  ...commandRegistry.filter(
    (command) =>
      command.id !== 'navigation.open' &&
      command.id !== 'selection.first' &&
      command.id !== 'selection.last',
  ),
]
export type ProductCommandId = (typeof productCommandRegistry)[number]['id']

export const productHotkeySequenceBindings = productNavigationCommands.flatMap((command) =>
  command.bindings.flatMap((binding) =>
    binding.registration.kind === 'sequence'
      ? [{ commandId: command.id, sequence: binding.registration.sequence }]
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
