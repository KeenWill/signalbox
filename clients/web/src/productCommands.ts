import { type CommandContext, type CommandId, commandRegistry, invokeCommand } from './commands'

export interface ProductCommandContext extends CommandContext {
  navigate: (path: string) => void
  openArtifactInspector?: () => void
}

const productNavigationCommands = [
  {
    id: 'navigate.attention',
    title: 'Go to Attention',
    description: 'Open the operator intervention queue.',
    category: 'Navigate',
    bindings: [{ label: 'g a', registration: { kind: 'sequence', sequence: ['G', 'A'] } }],
    run: (context: ProductCommandContext) => context.navigate('/attention'),
  },
  {
    id: 'navigate.sessions',
    title: 'Go to Sessions',
    description: 'Open the bounded session workspace.',
    category: 'Navigate',
    bindings: [{ label: 'g s', registration: { kind: 'sequence', sequence: ['G', 'S'] } }],
    run: (context: ProductCommandContext) => context.navigate('/sessions'),
  },
  {
    id: 'navigate.activity',
    title: 'Go to Activity',
    description: 'Open the system-wide event stream.',
    category: 'Navigate',
    bindings: [],
    run: (context: ProductCommandContext) => context.navigate('/activity'),
  },
  {
    id: 'navigate.imports',
    title: 'Go to Imports',
    description: 'Open conversation import operations.',
    category: 'Navigate',
    bindings: [],
    run: (context: ProductCommandContext) => context.navigate('/imports'),
  },
  {
    id: 'navigate.reviews',
    title: 'Go to Reviews',
    description: 'Open approval work and history.',
    category: 'Navigate',
    bindings: [],
    run: (context: ProductCommandContext) => context.navigate('/reviews'),
  },
  {
    id: 'navigate.runners',
    title: 'Go to Runners',
    description: 'Open runner capacity and health.',
    category: 'Navigate',
    bindings: [],
    run: (context: ProductCommandContext) => context.navigate('/runners'),
  },
  {
    id: 'navigate.search',
    title: 'Go to Search',
    description: 'Open cross-session search.',
    category: 'Navigate',
    bindings: [],
    run: (context: ProductCommandContext) => context.navigate('/search'),
  },
  {
    id: 'navigate.usage',
    title: 'Go to Usage',
    description: 'Open token and cost analysis.',
    category: 'Navigate',
    bindings: [],
    run: (context: ProductCommandContext) => context.navigate('/usage'),
  },
  {
    id: 'artifact.open',
    title: 'Open artifact inspector',
    description: 'Resolve and inspect an immutable blob by its declared identity.',
    category: 'Surface',
    bindings: [],
    available: (context: ProductCommandContext) => context.openArtifactInspector !== undefined,
    run: (context: ProductCommandContext) => context.openArtifactInspector?.(),
  },
  {
    id: 'navigate.settings',
    title: 'Go to Settings',
    description: 'Open browser-local workstation preferences.',
    category: 'Navigate',
    bindings: [{ label: 'g ,', registration: { kind: 'sequence', sequence: ['G', ','] } }],
    run: (context: ProductCommandContext) => context.navigate('/settings'),
  },
] as const

export const productCommandRegistry = [...productNavigationCommands, ...commandRegistry]
export type ProductCommandId = (typeof productCommandRegistry)[number]['id']

export const productHotkeySequenceBindings = productNavigationCommands.flatMap((command) =>
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
  else invokeCommand(id as CommandId, context)
}
