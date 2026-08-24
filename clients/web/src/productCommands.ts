import type { CommandBinding, CommandContext } from './commands'
import { commandRegistry, invokeCommand } from './commands'

export interface ProductCommandContext extends CommandContext {
  navigate: (path: string) => void
}

export const productCommandRegistry = commandRegistry.filter(
  (command) => command.id !== 'navigation.open',
)
export type ProductCommandId = (typeof productCommandRegistry)[number]['id']

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
  invokeCommand(id, context)
}
