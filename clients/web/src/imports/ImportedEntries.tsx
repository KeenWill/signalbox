import { useVirtualizer } from '@tanstack/react-virtual'
import { useEffect, useRef } from 'react'
import { type CommandContext, invokeCommand } from '../commands'
import type {
  WebImportContinuationReference,
  WebImportedEntry,
} from '../generated/web-contract.mjs'

// Tunable effective ceiling: imported evidence rows keep a small viewport-adjacent overscan.
const IMPORT_ENTRY_OVERSCAN_ROWS = 6

const safeAriaInteger = (value: string): number | undefined => {
  const parsed = BigInt(value)
  return parsed <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(parsed) : undefined
}

const sourceLabel = (entry: WebImportedEntry): string => {
  switch (entry.source_speaker) {
    case 'not_attested':
      return 'speaker not attested'
    case 'attested_absent':
      return 'speaker attested absent'
    case 'user':
      return 'source user role'
    case 'assistant':
      return 'source assistant role'
  }
}

const entryText = (entry: WebImportedEntry): string => {
  if (!entry.text) return entry.content_kind.replaceAll('_', ' ')
  switch (entry.text.kind) {
    case 'not_attested':
      return 'Text not attested by source'
    case 'attested_absent':
      return 'Text explicitly absent in source'
    case 'attested':
      return `${entry.text.leading_text}${entry.text.completeness === 'truncated' ? '…' : ''}`
  }
}

export function ImportedEntries({
  entries,
  logicalEntryCount,
  selected,
  commandContext,
}: {
  entries: readonly WebImportedEntry[]
  logicalEntryCount: string
  selected: WebImportContinuationReference | null
  commandContext: CommandContext
}) {
  const scrollRef = useRef<HTMLDivElement>(null)
  const virtualizer = useVirtualizer({
    count: entries.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 58,
    overscan: IMPORT_ENTRY_OVERSCAN_ROWS,
    getItemKey: (index) => entries[index]?.frontier.imported_entry_id ?? index,
  })
  const selectedIndex = entries.findIndex(
    (entry) => entry.frontier.imported_entry_id === selected?.imported_entry_id,
  )
  const virtualRows = virtualizer.getVirtualItems()
  useEffect(() => {
    if (selectedIndex >= 0) virtualizer.scrollToIndex(selectedIndex, { align: 'auto' })
  }, [selectedIndex, virtualizer])
  const onKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    const command = {
      ArrowDown: 'imports.entry.next',
      ArrowUp: 'imports.entry.previous',
      Home: 'imports.entry.first',
      End: 'imports.entry.last',
    }[event.key] as
      | 'imports.entry.next'
      | 'imports.entry.previous'
      | 'imports.entry.first'
      | 'imports.entry.last'
      | undefined
    if (!command) return
    event.preventDefault()
    invokeCommand(command, commandContext)
  }
  return (
    <div
      ref={scrollRef}
      className="import-entry-scroll"
      role="listbox"
      aria-label="Imported source entries"
      aria-activedescendant={selected ? `import-entry-${selected.imported_entry_id}` : undefined}
      tabIndex={0}
      onKeyDown={onKeyDown}
      data-mounted-rows={virtualRows.length}
      data-total-loaded={entries.length}
    >
      <div className="virtual-stage" style={{ height: virtualizer.getTotalSize() }}>
        {virtualRows.map((virtualRow) => {
          const entry = entries[virtualRow.index]
          if (!entry) return null
          const isSelected = selected?.imported_entry_id === entry.frontier.imported_entry_id
          return (
            // biome-ignore lint/a11y: Focus remains on the aria-activedescendant listbox.
            <div
              id={`import-entry-${entry.frontier.imported_entry_id}`}
              role="option"
              aria-selected={isSelected}
              aria-posinset={safeAriaInteger(entry.frontier.position)}
              aria-setsize={safeAriaInteger(logicalEntryCount)}
              className="import-entry-row"
              data-testid={`import-entry-${entry.frontier.position}`}
              key={entry.frontier.imported_entry_id}
              style={{
                height: virtualRow.size,
                transform: `translateY(${virtualRow.start}px)`,
              }}
              onClick={() =>
                invokeCommand('imports.entry.select', {
                  ...commandContext,
                  requestedImportEntry: entry.frontier.imported_entry_id,
                })
              }
            >
              <span className="import-position">
                {BigInt(entry.frontier.position).toLocaleString()}
              </span>
              <div>
                <strong>{sourceLabel(entry)}</strong>
                <p>{entryText(entry)}</p>
              </div>
              <span className="source-badge">Imported source</span>
            </div>
          )
        })}
      </div>
    </div>
  )
}
