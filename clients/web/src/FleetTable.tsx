import { flexRender, getCoreRowModel, useReactTable, type ColumnDef } from '@tanstack/react-table'
import { useVirtualizer } from '@tanstack/react-virtual'
import { useEffect, useMemo, useRef } from 'react'
import type { FleetRow } from './platform'
import { actions, useAppDispatch } from './state'

// Tunable effective ceiling: a small overscan keeps table DOM work near the viewport.
const TABLE_OVERSCAN_ROWS = 8

export function FleetTable({ rows, totalCount }: { rows: FleetRow[]; totalCount: number }) {
  'use no memo'
  const dispatch = useAppDispatch()
  const columns = useMemo<ColumnDef<FleetRow>[]>(() => [
    { accessorKey: 'repository', header: 'Repository / worktree' },
    {
      accessorKey: 'state',
      header: 'State',
      cell: ({ getValue }) => <span className={`status status-${String(getValue())}`}>{String(getValue())}</span>,
    },
    { accessorKey: 'purpose', header: 'Current purpose' },
    { accessorKey: 'age', header: 'Age' },
  ], [])
  const table = useReactTable({ data: rows, columns, getCoreRowModel: getCoreRowModel() })
  const tableRows = table.getRowModel().rows
  const parentRef = useRef<HTMLDivElement>(null)
  const virtualizer = useVirtualizer({
    count: tableRows.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 34,
    overscan: TABLE_OVERSCAN_ROWS,
    getItemKey: (index) => tableRows[index]?.original.id ?? index,
  })
  const virtualRows = virtualizer.getVirtualItems()
  const range: [number, number] = [virtualRows[0]?.index ?? 0, virtualRows.at(-1)?.index ?? 0]

  useEffect(() => {
    dispatch(actions.tableRangeSet(range))
  }, [dispatch, range[0], range[1]])

  return (
    <section className="table-panel" aria-labelledby="fleet-heading">
      <header className="section-header table-heading">
        <div><span className="eyebrow">Operator view</span><h2 id="fleet-heading">Fleet obligations</h2></div>
        <span className="window-count">{totalCount.toLocaleString()} logical · {rows.length} loaded</span>
      </header>
      <div className="data-table" role="table" aria-label="Fleet obligations">
        <div className="table-header" role="row">
          {table.getHeaderGroups()[0]?.headers.map((header) => (
            <div role="columnheader" key={header.id}>
              {flexRender(header.column.columnDef.header, header.getContext())}
            </div>
          ))}
        </div>
        <div
          ref={parentRef}
          className="virtual-scroll table-scroll"
          role="rowgroup"
          data-mounted-rows={virtualRows.length}
          data-total-loaded={rows.length}
          data-logical-total={totalCount}
        >
          <div className="virtual-stage" style={{ height: virtualizer.getTotalSize() }}>
            {virtualRows.map((virtualRow) => {
              const row = tableRows[virtualRow.index]
              if (!row) return null
              return (
                <div
                  className="table-row"
                  role="row"
                  key={row.original.id}
                  data-testid={`fleet-${row.original.id}`}
                  style={{ height: virtualRow.size, transform: `translateY(${virtualRow.start}px)` }}
                >
                  {row.getVisibleCells().map((cell) => (
                    <div role="cell" key={cell.id}>{flexRender(cell.column.columnDef.cell, cell.getContext())}</div>
                  ))}
                </div>
              )
            })}
          </div>
        </div>
      </div>
    </section>
  )
}
