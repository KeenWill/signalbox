import { defaultRangeExtractor, useVirtualizer } from '@tanstack/react-virtual'
import { useEffect, useRef } from 'react'
import { type CommandContext, invokeCommand } from '../commands'
import type {
  WebImportContinuationReference,
  WebImportedEntry,
} from '../generated/web-contract.mjs'
import { enumLabel } from '../labels'

// Tunable effective ceiling: imported evidence rows keep a small viewport-adjacent overscan.
const IMPORT_ENTRY_OVERSCAN_ROWS = 6

const sourceLabel = (entry: WebImportedEntry): string => {
  switch (entry.source_speaker) {
    case 'not_attested':
      return 'Speaker unknown'
    case 'attested_absent':
      return 'No speaker'
    case 'user':
      return 'User'
    case 'assistant':
      return 'Assistant'
  }
}

const entryText = (entry: WebImportedEntry): string => {
  if (!entry.text) return enumLabel(entry.content_kind)
  switch (entry.text.kind) {
    case 'not_attested':
      return 'No text recorded'
    case 'attested_absent':
      return 'No text'
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
  logicalEntryCount: number
  selected: WebImportContinuationReference | null
  commandContext: CommandContext
}) {
  const scrollRef = useRef<HTMLDivElement>(null)
  const selectedIndex = entries.findIndex(
    (entry) => entry.frontier.imported_entry_id === selected?.imported_entry_id,
  )
  const virtualizer = useVirtualizer({
    count: entries.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 58,
    overscan: IMPORT_ENTRY_OVERSCAN_ROWS,
    getItemKey: (index) => entries[index]?.frontier.imported_entry_id ?? index,
    rangeExtractor: (range) => {
      const indexes = defaultRangeExtractor(range)
      if (selectedIndex < 0 || indexes.includes(selectedIndex)) return indexes
      return [...indexes, selectedIndex].sort((left, right) => left - right)
    },
  })
  const virtualRows = virtualizer.getVirtualItems()
  useEffect(() => {
    if (selectedIndex >= 0) virtualizer.scrollToIndex(selectedIndex, { align: 'auto' })
  }, [selectedIndex, virtualizer])
  const onKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (!commandContext.canSelectImportEntry || commandContext.getState().app.overlay !== null)
      return
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
      aria-disabled={!commandContext.canSelectImportEntry}
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
              aria-disabled={!commandContext.canSelectImportEntry}
              aria-posinset={entry.frontier.position}
              aria-setsize={logicalEntryCount}
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
              <span className="import-position">{entry.frontier.position.toLocaleString()}</span>
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
