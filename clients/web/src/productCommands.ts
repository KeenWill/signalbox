import type { HotkeySequence } from '@tanstack/react-hotkeys'
import type { CommandBinding, CommandContext, CommandId } from './commands'
import { commandRegistry, invokeCommand } from './commands'
import { actions } from './state'

export interface ProductCommandContext extends CommandContext {
  navigate: (path: string) => void
  openNavigation: () => void
  sidebarAvailable?: boolean
}

const productNavigationCommands = [
  {
    id: 'session.open-by-id',
    title: 'Open session by id',
    description: 'Open a session using its identifier.',
    category: 'Navigate',
    bindings: [],
    available: (context: ProductCommandContext) => !context.navigationLocked,
    run: (context: ProductCommandContext) => context.dispatch(actions.overlaySet('session-entry')),
  },
  {
    id: 'navigation.toggle',
    title: 'Toggle sidebar',
    description: 'Collapse or expand the sidebar.',
    category: 'Surface',
    bindings: [],
    available: (context: ProductCommandContext) => context.sidebarAvailable === true,
    run: (context: ProductCommandContext) => context.dispatch(actions.navigationToggled()),
  },
  {
    id: 'navigation.open',
    title: 'Open product navigation',
    description: 'Choose a page.',
    category: 'Surface',
    bindings: [],
    run: (context: ProductCommandContext) => context.openNavigation(),
  },
  {
    id: 'navigate.attention',
    title: 'Go to Attention',
    description: 'Open Attention.',
    category: 'Navigate',
    bindings: [
      {
        label: 'g a',
        registration: { kind: 'sequence', sequence: ['G', 'A'] as HotkeySequence },
      },
    ],
    available: (context: ProductCommandContext) => !context.navigationLocked,
    run: (context: ProductCommandContext) => context.navigate('/attention'),
  },
  {
    id: 'navigate.sessions',
    title: 'Go to Sessions',
    description: 'Open Sessions.',
    category: 'Navigate',
    bindings: [
      {
        label: 'g s',
        registration: { kind: 'sequence', sequence: ['G', 'S'] as HotkeySequence },
      },
    ],
    available: (context: ProductCommandContext) => !context.navigationLocked,
    run: (context: ProductCommandContext) => context.navigate('/sessions'),
  },
  {
    id: 'navigate.imports',
    title: 'Go to Imports',
    description: 'Open Imports.',
    category: 'Navigate',
    bindings: [],
    available: (context: ProductCommandContext) => !context.navigationLocked,
    run: (context: ProductCommandContext) => context.navigate('/imports'),
  },
  {
    id: 'navigate.reviews',
    title: 'Go to Reviews',
    description: 'Open Reviews.',
    category: 'Navigate',
    bindings: [],
    available: (context: ProductCommandContext) => !context.navigationLocked,
    run: (context: ProductCommandContext) => context.navigate('/reviews'),
  },
  {
    id: 'navigate.runners',
    title: 'Go to Runners',
    description: 'Open Runners.',
    category: 'Navigate',
    bindings: [],
    available: (context: ProductCommandContext) => !context.navigationLocked,
    run: (context: ProductCommandContext) => context.navigate('/runners'),
  },
  {
    id: 'navigate.search',
    title: 'Go to Search',
    description: 'Open cross-session search.',
    category: 'Navigate',
    bindings: [],
    available: (context: ProductCommandContext) => !context.navigationLocked,
    run: (context: ProductCommandContext) => context.navigate('/search'),
  },
  {
    id: 'navigate.usage',
    title: 'Go to Usage',
    description: 'Open Usage.',
    category: 'Navigate',
    bindings: [],
    available: (context: ProductCommandContext) => !context.navigationLocked,
    run: (context: ProductCommandContext) => context.navigate('/usage'),
  },
  {
    id: 'navigate.settings',
    title: 'Go to Settings',
    description: 'Open Settings.',
    category: 'Navigate',
    bindings: [
      {
        label: 'g ,',
        registration: { kind: 'sequence', sequence: ['G', ','] as HotkeySequence },
      },
    ],
    available: (context: ProductCommandContext) => !context.navigationLocked,
    run: (context: ProductCommandContext) => context.navigate('/settings'),
  },
  {
    id: 'navigate.scenario',
    title: 'Go to Scenario studio',
    description: 'Open Scenario studio.',
    category: 'Navigate',
    bindings: [],
    available: (context: ProductCommandContext) => !context.navigationLocked,
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
  if (!productCommandAvailable(id, context)) return
  const navigationCommand = productNavigationCommands.find((command) => command.id === id)
  if (navigationCommand) navigationCommand.run(context)
  else invokeCommand(id as CommandId, context)
}
