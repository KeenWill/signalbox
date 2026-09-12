import { defaultRangeExtractor, useVirtualizer } from '@tanstack/react-virtual'
import { AlertTriangle, Bot, CheckCircle2, CircleDot, TerminalSquare } from 'lucide-react'
import { type ReactNode, useCallback, useEffect, useLayoutEffect, useMemo, useRef } from 'react'
import './session-transcript.css'
import { type CommandContext, invokeCommand } from './commands'
import type { TimelineItem, TimelineKind } from './platform'
import type { DetailMode } from './state'
import { actions, useAppDispatch, useAppSelector } from './state'

// Tunable effective ceiling: a small overscan prevents scroll gaps without mounting the window.
const TRANSCRIPT_OVERSCAN_ROWS = 7

interface RendererProps {
  item: TimelineItem
  detail: DetailMode
}

const toolBody = (item: TimelineItem, detail: DetailMode): string => {
  switch (detail) {
    case 'full':
      return item.body
    case 'condensed':
    case 'results':
      return item.body.split(' with ')[0] ?? item.body
  }
}

const renderers: Record<TimelineKind, (props: RendererProps) => React.JSX.Element> = {
  origin: ({ item }) => (
    <>
      <Bot aria-hidden="true" />
      <div>
        <strong>{item.label}</strong>
        <p>{item.body}</p>
      </div>
    </>
  ),
  progress: ({ item }) => (
    <>
      <CircleDot aria-hidden="true" />
      <div>
        <strong>{item.label}</strong>
        <p>{item.body}</p>
      </div>
    </>
  ),
  tool: ({ item, detail }) => (
    <>
      <TerminalSquare aria-hidden="true" />
      <div>
        <strong>{item.label}</strong>
        <p>{toolBody(item, detail)}</p>
      </div>
    </>
  ),
  result: ({ item }) => (
    <>
      <CheckCircle2 aria-hidden="true" />
      <div>
        <strong>{item.label}</strong>
        <p>{item.body}</p>
      </div>
    </>
  ),
  unknown: ({ item }) => (
    <>
      <AlertTriangle aria-hidden="true" />
      <div>
        <strong>{item.label}</strong>
        <p>{item.body}</p>
      </div>
    </>
  ),
}

const visibleInResults: Record<TimelineKind, boolean> = {
  origin: true,
  progress: false,
  tool: false,
  result: true,
  unknown: true,
}

export const visibleTimeline = (items: TimelineItem[], detail: DetailMode): TimelineItem[] => {
  switch (detail) {
    case 'full':
    case 'condensed':
      return items
    case 'results':
      return items.filter((item) => visibleInResults[item.kind])
  }
}

export function Transcript({
  items,
  context,
  autoFocus = false,
}: {
  items: TimelineItem[]
  context: CommandContext
  autoFocus?: boolean
}) {
  'use no memo'
  const dispatch = useAppDispatch()
  const detail = useAppSelector((state) => state.app.detail)
  const density = useAppSelector((state) => state.app.density)
  const selectedId = useAppSelector((state) => state.app.selectedTimeline)
  const visibleItems = useMemo(() => visibleTimeline(items, detail), [detail, items])
  const ids = useMemo(() => visibleItems.map((item) => item.id), [visibleItems])
  const reportRange = useCallback(
    (start: number, end: number) => {
      dispatch(actions.transcriptRangeSet({ start, end }))
    },
    [dispatch],
  )
  const firstVisibleId = visibleItems[0]?.id ?? null
  const selected = visibleItems.findIndex((item) => item.id === selectedId)
  useEffect(() => {
    if (selected < 0 && visibleItems.length > 0) {
      dispatch(actions.timelineSelected(firstVisibleId))
    }
  }, [dispatch, firstVisibleId, selected, visibleItems.length])

  const handleListboxKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    const command = {
      ArrowDown: 'selection.next',
      ArrowUp: 'selection.previous',
      Home: 'selection.first',
      End: 'selection.last',
    }[event.key] as
      | 'selection.next'
      | 'selection.previous'
      | 'selection.first'
      | 'selection.last'
      | undefined
    if (!command) return
    event.preventDefault()
    invokeCommand(command, context)
  }

  return (
    <section className="transcript-panel" aria-labelledby="timeline-heading">
      <header className="section-header">
        <div>
          <span className="eyebrow">Current session</span>
          <h1 id="timeline-heading">Timeline</h1>
        </div>
        <span className="window-count">{visibleItems.length} loaded</span>
      </header>
      <VirtualTranscript
        ids={ids}
        selectedId={selectedId}
        estimateSize={density === 'compact' ? 62 : 78}
        autoFocus={autoFocus}
        onRange={reportRange}
        className="virtual-scroll transcript-scroll"
        role="listbox"
        aria-label="Session timeline"
        onKeyDown={handleListboxKeyDown}
        renderRow={(index, measure, style) => {
          const item = visibleItems[index]
          if (!item) return null
          const Renderer = renderers[item.kind]
          return (
            // biome-ignore lint/a11y: Focus stays on the aria-activedescendant listbox; pointer selection is supplemental.
            <div
              id={item.id}
              key={item.id}
              role="option"
              aria-selected={selectedId === item.id}
              aria-posinset={index + 1}
              aria-setsize={visibleItems.length}
              className={`timeline-row kind-${item.kind}`}
              data-testid={`timeline-${item.id}`}
              ref={measure}
              data-index={index}
              style={style}
              onClick={() => dispatch(actions.timelineSelected(item.id))}
            >
              <span className="turn-rail">T{item.turn}</span>
              <div className="timeline-content">
                <Renderer item={item} detail={detail} />
              </div>
              <span className="elapsed">{item.elapsed}</span>
            </div>
          )
        }}
      />
    </section>
  )
}

export function VirtualTranscript({
  ids,
  renderRow,
  selectedId,
  estimateSize = 100,
  className = 'session-transcript-scroll',
  autoFocus = false,
  initialEnd = false,
  followEnd = false,
  onRange,
  onEdge,
  onKeyDown,
  role = 'region',
  'aria-label': label = 'Session transcript',
}: {
  ids: readonly string[]
  renderRow: (
    index: number,
    measure: (node: HTMLDivElement | null) => void,
    style: React.CSSProperties,
  ) => ReactNode
  selectedId?: string | null
  estimateSize?: number
  className?: string
  autoFocus?: boolean
  initialEnd?: boolean
  followEnd?: boolean
  onRange?: (start: number, end: number) => void
  onEdge?: (direction: 'before' | 'after') => void
  onKeyDown?: React.KeyboardEventHandler<HTMLDivElement>
  role?: 'listbox' | 'region'
  'aria-label'?: string
}) {
  'use no memo'
  const parent = useRef<HTMLDivElement>(null)
  const selected = selectedId ? ids.indexOf(selectedId) : -1
  const anchor = useRef<{ id: string; offset: number } | null>(null)
  const initialized = useRef(false)
  const atEnd = useRef(initialEnd)
  const previousLast = useRef<string | undefined>(undefined)
  const touchStart = useRef<number | null>(null)
  const virtualizer = useVirtualizer({
    count: ids.length,
    getScrollElement: () => parent.current,
    estimateSize: () => estimateSize,
    overscan: TRANSCRIPT_OVERSCAN_ROWS,
    getItemKey: (index) => ids[index] ?? index,
    rangeExtractor: (range) => {
      const indexes = defaultRangeExtractor(range)
      return selected < 0 || indexes.includes(selected)
        ? indexes
        : [...indexes, selected].sort((a, b) => a - b)
    },
  })
  const rows = virtualizer.getVirtualItems()
  const start = virtualizer.range?.startIndex ?? 0
  const end = virtualizer.range?.endIndex ?? 0
  useEffect(() => {
    onRange?.(
      Math.max(0, start - TRANSCRIPT_OVERSCAN_ROWS),
      Math.min(ids.length - 1, end + TRANSCRIPT_OVERSCAN_ROWS),
    )
  }, [start, end, ids.length, onRange])
  useEffect(() => {
    if (autoFocus) parent.current?.focus()
  }, [autoFocus])
  useEffect(() => {
    if (selected >= 0) virtualizer.scrollToIndex(selected, { align: 'auto' })
  }, [selected, virtualizer])
  useLayoutEffect(() => {
    if (!ids.length) return
    if (!initialized.current) {
      initialized.current = true
      if (initialEnd && selected < 0) virtualizer.scrollToIndex(ids.length - 1, { align: 'end' })
    } else if (
      followEnd &&
      atEnd.current &&
      previousLast.current &&
      ids.includes(previousLast.current)
    ) {
      virtualizer.scrollToIndex(ids.length - 1, { align: 'end' })
    } else if (anchor.current) {
      const index = ids.indexOf(anchor.current.id)
      const position = index < 0 ? undefined : virtualizer.getOffsetForIndex(index, 'start')
      if (position) virtualizer.scrollToOffset(position[0] + anchor.current.offset)
    }
    previousLast.current = ids.at(-1)
  }, [ids, initialEnd, followEnd, selected, virtualizer])
  const remember = () => {
    const offset = parent.current?.scrollTop ?? 0
    const row = virtualizer.getVirtualItems().find((item) => item.end > offset)
    anchor.current = row ? { id: String(row.key), offset: offset - row.start } : null
  }
  const edge = (direction?: 'before' | 'after') => {
    const element = parent.current
    if (!element) return
    remember()
    atEnd.current =
      direction !== 'before' && element.scrollHeight - element.scrollTop - element.clientHeight <= 1
    if (element.scrollTop < estimateSize && direction !== 'after') onEdge?.('before')
    else if (
      element.scrollHeight - element.scrollTop - element.clientHeight < estimateSize &&
      direction !== 'before'
    )
      onEdge?.('after')
  }
  return (
    // biome-ignore lint/a11y: The typed role is either an interactive listbox or a named, keyboard-scrollable region.
    <div
      ref={parent}
      className={className}
      role={role}
      aria-label={label}
      tabIndex={0}
      aria-activedescendant={
        role === 'listbox' && selected >= 0 ? (selectedId ?? undefined) : undefined
      }
      onKeyDown={(event) => {
        onKeyDown?.(event)
        if (event.target !== event.currentTarget || event.defaultPrevented) return
        if (['ArrowUp', 'PageUp', 'Home'].includes(event.key)) edge('before')
        if (['ArrowDown', 'PageDown', 'End'].includes(event.key)) edge('after')
      }}
      onTouchStart={(event) => {
        touchStart.current = event.touches[0]?.clientY ?? null
      }}
      onTouchMove={(event) => {
        const position = event.touches[0]?.clientY
        if (position !== undefined && touchStart.current !== null)
          edge(position > touchStart.current ? 'before' : 'after')
      }}
      onScroll={() => edge()}
      onWheel={(event) => edge(event.deltaY < 0 ? 'before' : 'after')}
      data-mounted-rows={rows.length}
      data-total-loaded={ids.length}
    >
      <div className="virtual-stage" style={{ height: virtualizer.getTotalSize() }}>
        {rows.map((row) =>
          renderRow(row.index, virtualizer.measureElement, {
            position: 'absolute',
            width: '100%',
            transform: `translateY(${row.start}px)`,
          }),
        )}
      </div>
    </div>
  )
}
