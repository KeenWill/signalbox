import { flexRender } from '@tanstack/react-table'
import { getCoreRowModel, type LegacyColumnDef, useLegacyTable } from '@tanstack/react-table/legacy'
import { useVirtualizer } from '@tanstack/react-virtual'
import { useMemo, useRef } from 'react'
import type { WebImportSummary } from '../generated/web-contract.mjs'

// Tunable effective ceiling: enough overscan for smooth dense-table keyboard scrolling.
const IMPORT_TABLE_OVERSCAN_ROWS = 7

const formatLabel = (format: WebImportSummary['format']): string => {
  switch (format) {
    case 'claude_code_session_jsonl_v1':
      return 'Claude Code · converter 1'
    case 'claude_code_session_jsonl_v2':
      return 'Claude Code · converter 2'
    case 'codex_rollout_jsonl_v1':
      return 'Codex rollout · converter 1'
  }
}

export function ImportsTable({
  rows,
  selectedId,
  onSelect,
}: {
  rows: readonly WebImportSummary[]
  selectedId: string | null
  onSelect: (id: string) => void
}) {
  'use no memo'
  const columns = useMemo<LegacyColumnDef<WebImportSummary>[]>(
    () => [
      {
        accessorKey: 'display_title',
        header: 'Import',
        cell: ({ row }) => (
          <button
            type="button"
            className="import-select"
            aria-label={`Inspect ${row.original.display_title ?? row.original.imported_conversation_id}`}
            onClick={() => onSelect(row.original.imported_conversation_id)}
          >
            <strong>{row.original.display_title ?? 'Untitled import'}</strong>
            <small>{row.original.imported_conversation_id}</small>
          </button>
        ),
      },
      {
        accessorKey: 'format',
        header: 'Source / converter',
        cell: ({ row }) => formatLabel(row.original.format),
      },
      {
        accessorKey: 'entry_count',
        header: 'Entries',
        cell: ({ row }) => BigInt(row.original.entry_count).toLocaleString(),
      },
      {
        accessorKey: 'source_session_id',
        header: 'Source session evidence',
        cell: ({ row }) => {
          const evidence = row.original.source_session_id
          if (!evidence) return 'Not attested'
          return `${evidence.leading_text}${evidence.completeness === 'truncated' ? '…' : ''}`
        },
      },
    ],
    [onSelect],
  )
  const table = useLegacyTable({
    data: [...rows],
    columns,
    getCoreRowModel: getCoreRowModel(),
  })
  const tableRows = table.getRowModel().rows
  const scrollRef = useRef<HTMLDivElement>(null)
  const virtualizer = useVirtualizer({
    count: tableRows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 42,
    overscan: IMPORT_TABLE_OVERSCAN_ROWS,
    getItemKey: (index) => tableRows[index]?.original.imported_conversation_id ?? index,
  })
  const virtualRows = virtualizer.getVirtualItems()

  return (
    // biome-ignore lint/a11y/useSemanticElements: Virtual rows need a scrollable ARIA table container.
    <div className="imports-table" role="table" aria-label="Imported conversations">
      {/* biome-ignore lint/a11y/useFocusableInteractive lint/a11y/useSemanticElements: The read-only virtual header row delegates focus to selectable body controls. */}
      <div className="imports-table-header" role="row" aria-rowindex={1}>
        {table.getHeaderGroups()[0]?.headers.map((header) => (
          // biome-ignore lint/a11y/useFocusableInteractive: Read-only virtual headers do not need independent focus.
          // biome-ignore lint/a11y/useSemanticElements: Virtualized headers cannot use native table cells.
          <div role="columnheader" key={header.id}>
            {flexRender(header.column.columnDef.header, header.getContext())}
          </div>
        ))}
      </div>
      {/* biome-ignore lint/a11y/useSemanticElements: A virtual row group cannot use a native tbody. */}
      <div
        ref={scrollRef}
        className="imports-table-scroll"
        role="rowgroup"
        aria-label="Imported conversation rows"
        // biome-ignore lint/a11y/noNoninteractiveTabindex: The virtual row group is the keyboard-scroll viewport.
        tabIndex={0}
        data-mounted-rows={virtualRows.length}
        data-total-loaded={rows.length}
      >
        <div className="virtual-stage" style={{ height: virtualizer.getTotalSize() }}>
          {virtualRows.map((virtualRow) => {
            const row = tableRows[virtualRow.index]
            if (!row) return null
            const selected = selectedId === row.original.imported_conversation_id
            return (
              // biome-ignore lint/a11y: Focusable selection lives in the row's named button.
              <div
                role="row"
                aria-rowindex={virtualRow.index + 2}
                aria-selected={selected}
                className="imports-table-row"
                data-testid={`import-row-${row.original.imported_conversation_id}`}
                key={row.original.imported_conversation_id}
                style={{
                  height: virtualRow.size,
                  transform: `translateY(${virtualRow.start}px)`,
                }}
              >
                {row.getVisibleCells().map((cell) => (
                  // biome-ignore lint/a11y/useSemanticElements: Virtualized cells cannot be native table cells.
                  <div role="cell" key={cell.id}>
                    {flexRender(cell.column.columnDef.cell, cell.getContext())}
                  </div>
                ))}
              </div>
            )
          })}
        </div>
      </div>
    </div>
  )
}
