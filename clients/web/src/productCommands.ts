import type { HotkeySequence } from '@tanstack/react-hotkeys'
import type { CommandBinding, CommandContext, CommandId } from './commands'
import { commandRegistry, invokeCommand } from './commands'

export interface ProductCommandContext extends CommandContext {
  navigate: (path: string) => void
  openNavigation: () => void
}

const productNavigationCommands = [
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
    bindings: [
      {
        label: 'g a',
        registration: { kind: 'sequence', sequence: ['G', 'A'] as HotkeySequence },
      },
    ],
    run: (context: ProductCommandContext) => context.navigate('/attention'),
  },
  {
    id: 'navigate.sessions',
    title: 'Go to Sessions',
    description: 'Open the bounded session workspace.',
    category: 'Navigate',
    bindings: [
      {
        label: 'g s',
        registration: { kind: 'sequence', sequence: ['G', 'S'] as HotkeySequence },
      },
    ],
    run: (context: ProductCommandContext) => context.navigate('/sessions'),
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
    id: 'navigate.settings',
    title: 'Go to Settings',
    description: 'Open browser-local workstation preferences.',
    category: 'Navigate',
    bindings: [
      {
        label: 'g ,',
        registration: { kind: 'sequence', sequence: ['G', ','] as HotkeySequence },
      },
    ],
    run: (context: ProductCommandContext) => context.navigate('/settings'),
  },
  {
    id: 'navigate.scenario',
    title: 'Go to Scenario Studio',
    description: 'Open the streaming interaction scenario.',
    category: 'Navigate',
    bindings: [],
    run: (context: ProductCommandContext) => context.navigate('/scenario/streaming'),
  },
] as const

export const productCommandRegistry = [
  ...productNavigationCommands,
  ...commandRegistry.filter(
    (command) => command.id !== 'navigation.open' && !command.id.startsWith('navigate.'),
  ),
]
export type ProductCommandId = (typeof productCommandRegistry)[number]['id']

export const productCommandAvailable = (
  id: ProductCommandId,
  context: ProductCommandContext,
): boolean => {
  const command = productCommandRegistry.find((candidate) => candidate.id === id)
  return command !== undefined && (!('available' in command) || command.available(context))
}

export const productHotkeyBindings = productCommandRegistry.flatMap((command) => {
  const bindings: readonly CommandBinding[] = command.bindings
  return bindings.flatMap((binding) =>
    binding.registration?.kind === 'hotkey'
      ? [{ commandId: command.id, hotkey: binding.registration.hotkey }]
      : [],
  )
})

export const productHotkeySequenceBindings = productCommandRegistry.flatMap((command) => {
  const bindings: readonly CommandBinding[] = command.bindings
  return bindings.flatMap((binding) =>
    binding.registration?.kind === 'sequence'
      ? [{ commandId: command.id, sequence: binding.registration.sequence }]
      : [],
  )
})

export const invokeProductCommand = (
  id: ProductCommandId,
  context: ProductCommandContext,
): void => {
  const navigationCommand = productNavigationCommands.find((command) => command.id === id)
  if (navigationCommand) navigationCommand.run(context)
  else invokeCommand(id as CommandId, context)
}
