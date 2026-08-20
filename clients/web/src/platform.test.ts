import { describe, expect, it } from 'vitest'
import {
  SCENARIO_FLEET_WINDOW_ITEMS,
  SCENARIO_TIMELINE_WINDOW_ITEMS,
  ScenarioTransport,
} from './platform'

describe('ScenarioTransport', () => {
  it('bounds a six-figure timeline request', async () => {
    const transport = new ScenarioTransport('large-timeline')
    const window = await transport.readTimeline({ limit: 100_000 })

    expect(window.totalCount).toBe(transport.scenario.timelineTotal)
    expect(window.items).toHaveLength(SCENARIO_TIMELINE_WINDOW_ITEMS)
    expect(window.nextCursor).toBe(window.items.at(-1)?.cursor)
  })

  it('uses stable logical cursors for adjacent windows', async () => {
    const transport = new ScenarioTransport('large-table')
    const first = await transport.readFleet({ limit: 2 })
    const second = await transport.readFleet({ after: first.nextCursor, limit: 2 })

    expect(first.items.map((row) => row.cursor)).toEqual(['fleet:0', 'fleet:1'])
    expect(second.items.map((row) => row.cursor)).toEqual(['fleet:2', 'fleet:3'])
  })

  it('normalizes fractional timeline limits before constructing cursors', async () => {
    const transport = new ScenarioTransport('streaming')
    const window = await transport.readTimeline({ limit: 2.9 })

    expect(window.items).toHaveLength(2)
    expect(window.nextCursor).toBe(window.items.at(-1)?.cursor)
  })

  it('uses the minimum fleet limit for non-finite input', async () => {
    const transport = new ScenarioTransport('streaming')
    const window = await transport.readFleet({ limit: Number.NaN })

    expect(window.items).toHaveLength(1)
    expect(window.nextCursor).toBe(window.items.at(-1)?.cursor)
  })

  it('clamps timeline limits to the minimum boundary', async () => {
    const transport = new ScenarioTransport('streaming')
    const window = await transport.readTimeline({ limit: 0 })

    expect(window.items).toHaveLength(1)
  })

  it('clamps fleet limits to the configured window ceiling', async () => {
    const transport = new ScenarioTransport('large-table')
    const window = await transport.readFleet({ limit: SCENARIO_FLEET_WINDOW_ITEMS + 1 })

    expect(window.items).toHaveLength(SCENARIO_FLEET_WINDOW_ITEMS)
  })

  it('returns an empty timeline window for a cursor beyond the scenario', async () => {
    const transport = new ScenarioTransport('streaming')
    const window = await transport.readTimeline({
      after: `timeline:${transport.scenario.timelineTotal}`,
      limit: 1,
    })

    expect(window.items).toEqual([])
    expect(window.nextCursor).toBeUndefined()
    expect(window.totalCount).toBe(transport.scenario.timelineTotal)
  })

  it('returns an empty fleet window for a cursor beyond the scenario', async () => {
    const transport = new ScenarioTransport('streaming')
    const window = await transport.readFleet({
      after: `fleet:${transport.scenario.tableTotal}`,
      limit: 1,
    })

    expect(window.items).toEqual([])
    expect(window.nextCursor).toBeUndefined()
    expect(window.totalCount).toBe(transport.scenario.tableTotal)
  })

  it('does not advance past an empty timeline cursor suffix', async () => {
    const transport = new ScenarioTransport('streaming')
    const window = await transport.readTimeline({ after: 'timeline:', limit: 1 })

    expect(window.items[0]?.cursor).toBe('timeline:0')
  })

  it('does not advance past a whitespace fleet cursor suffix', async () => {
    const transport = new ScenarioTransport('streaming')
    const window = await transport.readFleet({ after: 'fleet: ', limit: 1 })

    expect(window.items[0]?.cursor).toBe('fleet:0')
  })
})
