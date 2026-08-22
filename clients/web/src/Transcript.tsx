import { defaultRangeExtractor, useVirtualizer } from '@tanstack/react-virtual'
import { AlertTriangle, Bot, CheckCircle2, CircleDot, TerminalSquare } from 'lucide-react'
import { useEffect, useMemo, useRef } from 'react'
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
        <p>Safe generic renderer · {item.body}</p>
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

export function Transcript({ items, context }: { items: TimelineItem[]; context: CommandContext }) {
  'use no memo'
  const dispatch = useAppDispatch()
  const detail = useAppSelector((state) => state.app.detail)
  const density = useAppSelector((state) => state.app.density)
  const selectedId = useAppSelector((state) => state.app.selectedTimeline)
  const parentRef = useRef<HTMLDivElement>(null)
  const visibleItems = useMemo(() => visibleTimeline(items, detail), [detail, items])
  const firstVisibleId = visibleItems[0]?.id ?? null
  const selected = visibleItems.findIndex((item) => item.id === selectedId)
  const virtualizer = useVirtualizer({
    count: visibleItems.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => (density === 'compact' ? 62 : 78),
    overscan: TRANSCRIPT_OVERSCAN_ROWS,
    getItemKey: (index) => visibleItems[index]?.id ?? index,
    rangeExtractor: (range) => {
      const indexes = defaultRangeExtractor(range)
      if (selected < 0 || indexes.includes(selected)) return indexes
      return [...indexes, selected].sort((left, right) => left - right)
    },
  })
  const virtualItems = virtualizer.getVirtualItems()
  const rangeStart = Math.max((virtualizer.range?.startIndex ?? 0) - TRANSCRIPT_OVERSCAN_ROWS, 0)
  const rangeEnd = Math.min(
    (virtualizer.range?.endIndex ?? 0) + TRANSCRIPT_OVERSCAN_ROWS,
    Math.max(visibleItems.length - 1, 0),
  )

  useEffect(() => {
    dispatch(actions.transcriptRangeSet({ start: rangeStart, end: rangeEnd }))
  }, [dispatch, rangeEnd, rangeStart])

  useEffect(() => {
    if (selected >= 0) virtualizer.scrollToIndex(selected, { align: 'auto' })
  }, [selected, virtualizer])

  useEffect(() => {
    if (selected < 0 && visibleItems.length > 0) {
      dispatch(actions.timelineSelected(firstVisibleId))
    }
  }, [dispatch, firstVisibleId, selected, visibleItems.length])

  useEffect(() => {
    let innerFrame = 0
    const outerFrame = requestAnimationFrame(() => {
      innerFrame = requestAnimationFrame(() => parentRef.current?.focus())
    })
    return () => {
      cancelAnimationFrame(outerFrame)
      cancelAnimationFrame(innerFrame)
    }
  }, [])

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
          <h1 id="timeline-heading">Bounded timeline</h1>
        </div>
        <span className="window-count">{visibleItems.length} loaded</span>
      </header>
      <div
        ref={parentRef}
        className="virtual-scroll transcript-scroll"
        role="listbox"
        aria-label="Session timeline"
        aria-activedescendant={selected >= 0 ? (selectedId ?? undefined) : undefined}
        tabIndex={0}
        onKeyDown={handleListboxKeyDown}
        data-mounted-rows={virtualItems.length}
        data-total-loaded={visibleItems.length}
      >
        <div className="virtual-stage" style={{ height: virtualizer.getTotalSize() }}>
          {virtualItems.map((virtualRow) => {
            const item = visibleItems[virtualRow.index]
            if (!item) return null
            const Renderer = renderers[item.kind]
            return (
              // biome-ignore lint/a11y: Focus stays on the aria-activedescendant listbox; pointer selection is supplemental.
              <div
                id={item.id}
                key={item.id}
                role="option"
                aria-selected={selectedId === item.id}
                aria-posinset={virtualRow.index + 1}
                aria-setsize={visibleItems.length}
                className={`timeline-row kind-${item.kind}`}
                data-testid={`timeline-${item.id}`}
                ref={virtualizer.measureElement}
                data-index={virtualRow.index}
                style={{ transform: `translateY(${virtualRow.start}px)` }}
                onClick={() => dispatch(actions.timelineSelected(item.id))}
              >
                <span className="turn-rail">T{item.turn}</span>
                <div className="timeline-content">
                  <Renderer item={item} detail={detail} />
                </div>
                <span className="elapsed">{item.elapsed}</span>
              </div>
            )
          })}
        </div>
      </div>
    </section>
  )
}
