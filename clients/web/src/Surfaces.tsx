import * as Dialog from '@radix-ui/react-dialog'
import * as Tooltip from '@radix-ui/react-tooltip'
import { Command, Menu, Moon, PanelLeftClose, Rows3, Sun, X } from 'lucide-react'
import { type CommandContext, type CommandId, commandRegistry, invokeCommand } from './commands'
import type { ScenarioDefinition } from './platform'
import { ScenarioNavigation } from './ScenarioNavigation'
import { selectApp, useAppSelector, type VisibleRange } from './state'

export function IconCommand({
  id,
  context,
  label,
  children,
  className,
}: {
  id: CommandId
  context: CommandContext
  label: string
  children: React.ReactNode
  className?: string
}) {
  return (
    <Tooltip.Root>
      <Tooltip.Trigger asChild>
        <button
          type="button"
          className={className ?? 'icon-button'}
          aria-label={label}
          onClick={() => invokeCommand(id, context)}
        >
          {children}
        </button>
      </Tooltip.Trigger>
      <Tooltip.Portal>
        <Tooltip.Content className="tooltip" sideOffset={6}>
          {label}
        </Tooltip.Content>
      </Tooltip.Portal>
    </Tooltip.Root>
  )
}

function DialogFrame({
  open,
  title,
  description,
  onClose,
  children,
}: {
  open: boolean
  title: string
  description: string
  onClose: () => void
  children: React.ReactNode
}) {
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(nextOpen) => {
        if (!nextOpen) onClose()
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className="dialog-overlay" />
        <Dialog.Content className="dialog-content" aria-describedby="dialog-description">
          <div className="dialog-heading">
            <div>
              <Dialog.Title>{title}</Dialog.Title>
              <Dialog.Description id="dialog-description">{description}</Dialog.Description>
            </div>
            <Dialog.Close asChild>
              <button className="icon-button" type="button" aria-label={`Close ${title}`}>
                <X />
              </button>
            </Dialog.Close>
          </div>
          {children}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  )
}

export function OverlaySurfaces({
  context,
  activeId,
}: {
  context: CommandContext
  activeId: string
}) {
  const overlay = useAppSelector((state) => state.app.overlay)
  const close = () => invokeCommand('surface.escape', context)
  const availableCommands = commandRegistry.filter(
    (command) => command.id !== 'surface.escape' && command.available(context),
  )

  return (
    <>
      <DialogFrame
        open={overlay === 'palette'}
        title="Command palette"
        description="One registry powers buttons, menus, hotkeys, and this palette."
        onClose={close}
      >
        <div className="command-list">
          {availableCommands.map((command) => (
            <button
              key={command.id}
              type="button"
              onClick={() => {
                close()
                invokeCommand(command.id, context)
              }}
            >
              <span>
                <strong>{command.title}</strong>
                <small>{command.description}</small>
              </span>
              <kbd>{command.bindings[0]?.label ?? '—'}</kbd>
            </button>
          ))}
        </div>
      </DialogFrame>
      <DialogFrame
        open={overlay === 'help'}
        title="Keyboard help"
        description="Modal navigation pauses while a text field owns editing."
        onClose={close}
      >
        <dl className="shortcut-list">
          {availableCommands
            .filter((command) => command.bindings.length > 0)
            .map((command) => (
              <div key={command.id}>
                <dt>{command.title}</dt>
                <dd>
                  {command.bindings.map((binding) => (
                    <kbd key={binding.label}>{binding.label}</kbd>
                  ))}
                </dd>
              </div>
            ))}
        </dl>
      </DialogFrame>
      <DialogFrame
        open={overlay === 'navigation'}
        title="Development scenarios"
        description="Deterministic projections exercise the real client shell."
        onClose={close}
      >
        <ScenarioNavigation activeId={activeId} onSelect={close} />
      </DialogFrame>
    </>
  )
}

export function Toolbar({ context }: { context: CommandContext }) {
  const app = useAppSelector(selectApp)
  return (
    <div className="toolbar" role="toolbar" aria-label="Workspace controls">
      <IconCommand
        id="navigation.open"
        context={context}
        label="Open scenarios"
        className="icon-button mobile-only"
      >
        <Menu />
      </IconCommand>
      <fieldset className="segmented" aria-label="Transcript detail">
        {(['full', 'condensed', 'results'] as const).map((detail) => (
          <button
            type="button"
            key={detail}
            aria-pressed={app.detail === detail}
            onClick={() => invokeCommand(`detail.${detail}`, context)}
          >
            {detail}
          </button>
        ))}
      </fieldset>
      <IconCommand
        id="density.toggle"
        context={context}
        label={`Use ${app.density === 'compact' ? 'comfortable' : 'compact'} density`}
      >
        <Rows3 />
      </IconCommand>
      <IconCommand
        id="layout.toggle"
        context={context}
        label={`Switch to ${app.layout === 'focus' ? 'workbench' : 'focus'} layout`}
      >
        <PanelLeftClose />
      </IconCommand>
      <IconCommand
        id="theme.toggle"
        context={context}
        label={`Use ${app.theme === 'dark' ? 'light' : 'dark'} theme`}
      >
        {app.theme === 'dark' ? <Sun /> : <Moon />}
      </IconCommand>
      <IconCommand id="palette.open" context={context} label="Open command palette">
        <Command />
      </IconCommand>
    </div>
  )
}

export interface DiagnosticSnapshot {
  scenario: string
  connection: string
  loadedTimeline: number
  logicalTimeline: number
  loadedFleet: number
  logicalFleet: number
  transcriptRange: VisibleRange
  tableRange: VisibleRange
  queryStates: string[]
  queryCacheSize: number
  recentActions: readonly string[]
}

// Tunable effective ceiling: the inspector shows a concise recent action tail.
const VISIBLE_DIAGNOSTIC_ACTIONS = 8

export function Diagnostics({
  scenario,
  snapshot,
}: {
  scenario: ScenarioDefinition
  snapshot: DiagnosticSnapshot
}) {
  const app = useAppSelector(selectApp)
  return (
    <aside className="diagnostics" aria-labelledby="diagnostics-heading">
      <header>
        <span className="eyebrow">Read only · bounded</span>
        <h2 id="diagnostics-heading">Diagnostics</h2>
      </header>
      <dl>
        <div>
          <dt>Scenario</dt>
          <dd>{scenario.id}</dd>
        </div>
        <div>
          <dt>Connection</dt>
          <dd>
            <span className={`status status-${scenario.connection}`}>{scenario.connection}</span>
          </dd>
        </div>
        <div>
          <dt>Durable cursor</dt>
          <dd>timeline:{Math.max(snapshot.loadedTimeline - 1, 0)}</dd>
        </div>
        <div>
          <dt>Timeline window</dt>
          <dd>
            {snapshot.loadedTimeline} / {snapshot.logicalTimeline.toLocaleString()}
          </dd>
        </div>
        <div>
          <dt>Fleet window</dt>
          <dd>
            {snapshot.loadedFleet} / {snapshot.logicalFleet.toLocaleString()}
          </dd>
        </div>
        <div>
          <dt>Virtual timeline</dt>
          <dd>
            {app.transcriptRange.start}–{app.transcriptRange.end}
          </dd>
        </div>
        <div>
          <dt>Virtual table</dt>
          <dd>
            {app.tableRange.start}–{app.tableRange.end}
          </dd>
        </div>
        <div>
          <dt>Query cache</dt>
          <dd>{snapshot.queryCacheSize} bounded entries</dd>
        </div>
      </dl>
      <h3>Recent Redux actions</h3>
      <ol>
        {snapshot.recentActions.slice(-VISIBLE_DIAGNOSTIC_ACTIONS).map((action, index) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: Actions may repeat, so their bounded log position disambiguates them.
          <li key={`${action}-${index}`}>{action}</li>
        ))}
      </ol>
    </aside>
  )
}
