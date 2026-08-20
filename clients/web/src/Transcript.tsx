import { useVirtualizer } from '@tanstack/react-virtual'
import { AlertTriangle, Bot, CheckCircle2, CircleDot, TerminalSquare } from 'lucide-react'
import { useEffect, useMemo, useRef } from 'react'
import { actions, useAppDispatch, useAppSelector } from './state'
import type { DetailMode } from './state'
import type { TimelineItem, TimelineKind } from './platform'

// Tunable effective ceiling: a small overscan prevents scroll gaps without mounting the window.
const TRANSCRIPT_OVERSCAN_ROWS = 7

interface RendererProps {
  item: TimelineItem
  condensed: boolean
}

const renderers: Record<TimelineKind, (props: RendererProps) => React.JSX.Element> = {
  origin: ({ item }) => (
    <>
      <Bot aria-hidden="true" />
      <div><strong>{item.label}</strong><p>{item.body}</p></div>
    </>
  ),
  progress: ({ item }) => (
    <>
      <CircleDot aria-hidden="true" />
      <div><strong>{item.label}</strong><p>{item.body}</p></div>
    </>
  ),
  tool: ({ item, condensed }) => (
    <>
      <TerminalSquare aria-hidden="true" />
      <div><strong>{item.label}</strong><p>{condensed ? item.body.split(' with ')[0] : item.body}</p></div>
    </>
  ),
  result: ({ item }) => (
    <>
      <CheckCircle2 aria-hidden="true" />
      <div><strong>{item.label}</strong><p>{item.body}</p></div>
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

export const visibleTimeline = (items: TimelineItem[], detail: DetailMode): TimelineItem[] => {
  if (detail !== 'results') return items
  return items.filter((item) => ['origin', 'result', 'unknown'].includes(item.kind))
}

export function Transcript({ items }: { items: TimelineItem[] }) {
  'use no memo'
  const dispatch = useAppDispatch()
  const detail = useAppSelector((state) => state.app.detail)
  const density = useAppSelector((state) => state.app.density)
  const selected = useAppSelector((state) => state.app.selectedTimeline)
  const parentRef = useRef<HTMLDivElement>(null)
  const visibleItems = useMemo(() => visibleTimeline(items, detail), [detail, items])
  const virtualizer = useVirtualizer({
    count: visibleItems.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => (density === 'compact' ? 62 : 78),
    overscan: TRANSCRIPT_OVERSCAN_ROWS,
    getItemKey: (index) => visibleItems[index]?.id ?? index,
  })
  const virtualItems = virtualizer.getVirtualItems()
  const range: [number, number] = [
    virtualItems[0]?.index ?? 0,
    virtualItems.at(-1)?.index ?? 0,
  ]

  useEffect(() => {
    dispatch(actions.transcriptRangeSet(range))
  }, [dispatch, range[0], range[1]])

  useEffect(() => {
    if (selected < visibleItems.length) virtualizer.scrollToIndex(selected, { align: 'auto' })
  }, [selected, virtualizer, visibleItems.length])

  useEffect(() => {
    if (selected >= visibleItems.length && visibleItems.length > 0) {
      dispatch(actions.timelineSelected(visibleItems.length - 1))
    }
  }, [dispatch, selected, visibleItems.length])

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
        aria-activedescendant={visibleItems[selected]?.id}
        tabIndex={0}
        data-mounted-rows={virtualItems.length}
        data-total-loaded={visibleItems.length}
      >
        <div className="virtual-stage" style={{ height: virtualizer.getTotalSize() }}>
          {virtualItems.map((virtualRow) => {
            const item = visibleItems[virtualRow.index]
            if (!item) return null
            const Renderer = renderers[item.kind]
            return (
              <div
                id={item.id}
                key={item.id}
                role="option"
                aria-selected={selected === virtualRow.index}
                className={`timeline-row kind-${item.kind}`}
                data-testid={`timeline-${item.id}`}
                ref={virtualizer.measureElement}
                data-index={virtualRow.index}
                style={{ transform: `translateY(${virtualRow.start}px)` }}
                onClick={() => dispatch(actions.timelineSelected(virtualRow.index))}
              >
                <span className="turn-rail">T{item.turn}</span>
                <div className="timeline-content">
                  <Renderer item={item} condensed={detail !== 'full'} />
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
