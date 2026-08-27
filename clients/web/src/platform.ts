import { SESSION_FOUNDATION_TOTAL, sessionFoundationScenario } from './session-timeline/model'

export type TimelineKind = 'origin' | 'progress' | 'tool' | 'result' | 'unknown'

export interface TimelineItem {
  id: string
  cursor: string
  turn: number
  kind: TimelineKind
  label: string
  body: string
  elapsed: string
}

const FLEET_STATE_CYCLE = ['active', 'queued', 'blocked', 'settled'] as const
export type FleetState = (typeof FLEET_STATE_CYCLE)[number]

export interface FleetRow {
  id: string
  cursor: string
  repository: string
  state: FleetState
  purpose: string
  age: string
}

export interface WindowRequest {
  after?: string
  limit: number
}

export interface CursorWindow<T> {
  items: T[]
  nextCursor?: string
  totalCount: number
}

export const scenarios = [
  {
    id: 'streaming',
    title: 'Streaming session',
    description: 'Durable progress with an ephemeral provider draft.',
    connection: 'connected',
    timelineTotal: 240,
    tableTotal: 180,
  },
  {
    id: 'approval',
    title: 'Approval required',
    description: 'A pending tool operation needs operator attention.',
    connection: 'connected',
    timelineTotal: 180,
    tableTotal: 240,
  },
  {
    id: 'recovery',
    title: 'Recovery',
    description: 'A resynchronizing client preserves its durable cursor.',
    connection: 'recovering',
    timelineTotal: 320,
    tableTotal: 200,
  },
  {
    id: 'large-timeline',
    title: '100k timeline',
    description: 'A bounded window over a six-figure session history.',
    connection: 'connected',
    timelineTotal: 100_000,
    tableTotal: 500,
  },
  {
    id: 'session-foundation',
    title: 'Million-event session',
    description: 'Stable addresses and bounded windows over one enormous durable history.',
    connection: 'connected',
    timelineTotal: SESSION_FOUNDATION_TOTAL,
    tableTotal: 120,
  },
  {
    id: 'search-usage',
    title: 'Search and usage',
    description: 'Unloaded lexical hits and labeled model-call evidence at scale.',
    connection: 'connected',
    timelineTotal: 1_000_000,
    tableTotal: 240,
  },
  {
    id: 'large-table',
    title: 'Million-row fleet',
    description: 'A virtualized operator table over a million logical rows.',
    connection: 'connected',
    timelineTotal: 400,
    tableTotal: 1_000_000,
  },
  {
    id: 'huge-source',
    title: 'Huge source',
    description: 'Unknown and source-like records remain inspectable.',
    connection: 'connected',
    timelineTotal: 20_000,
    tableTotal: 300,
  },
  {
    id: 'imports',
    title: 'Million-row imports',
    description: 'Bounded discovery and immutable imported-entry windows.',
    connection: 'connected',
    timelineTotal: 250_000,
    tableTotal: 1_000_000,
  },
  {
    id: 'blobs',
    title: 'Blob evidence',
    description: 'Artifact summaries avoid eager binary materialization.',
    connection: 'connected',
    timelineTotal: 600,
    tableTotal: 120,
  },
  {
    id: 'attachments',
    title: 'Artifact attachments',
    description: 'Typed document, derivative, and media placeholders across attachment surfaces.',
    connection: 'connected',
    timelineTotal: 600,
    tableTotal: 120,
  },
  {
    id: 'responsive',
    title: 'Responsive shell',
    description: 'The same workspace at narrow and wide breakpoints.',
    connection: 'connected',
    timelineTotal: 200,
    tableTotal: 200,
  },
] as const

export type ScenarioDefinition = (typeof scenarios)[number]
export type ScenarioId = ScenarioDefinition['id']

export interface SignalboxTransport {
  readonly scenario: ScenarioDefinition
  readTimeline(request: WindowRequest): Promise<CursorWindow<TimelineItem>>
  readFleet(request: WindowRequest): Promise<CursorWindow<FleetRow>>
}

// Tunable effective ceiling: each development scenario loads one bounded timeline window.
export const SCENARIO_TIMELINE_WINDOW_ITEMS = 360
// Tunable effective ceiling: each development scenario loads one bounded fleet window.
export const SCENARIO_FLEET_WINDOW_ITEMS = 480

const normalizedLimit = ({
  requested,
  maximum,
}: {
  requested: number
  maximum: number
}): number => {
  if (!Number.isFinite(requested)) return 1
  return Math.min(Math.max(Math.trunc(requested), 1), maximum)
}

const parseCursor = (cursor: string | undefined, prefix: string): number => {
  if (!cursor?.startsWith(`${prefix}:`)) return 0
  const suffix = cursor.slice(prefix.length + 1)
  if (!/^(0|[1-9]\d*)$/.test(suffix)) return 0
  const parsed = Number(suffix)
  return Number.isSafeInteger(parsed) && parsed >= 0 ? parsed + 1 : 0
}

const fleetStateAt = (index: number): FleetState => {
  const state = FLEET_STATE_CYCLE[index % FLEET_STATE_CYCLE.length]
  if (state === undefined) throw new Error('Fleet state cycle must remain non-empty')
  return state
}

const timelineKind = (index: number): TimelineKind => {
  if (index % 17 === 0) return 'unknown'
  if (index % 11 === 0) return 'result'
  if (index % 5 === 0) return 'tool'
  if (index % 3 === 0) return 'progress'
  return 'origin'
}

interface TimelineCopy {
  label: string
  body: string
}

const timelineCopy = (kind: TimelineKind, index: number): TimelineCopy => {
  const serial = String(index + 1).padStart(5, '0')
  switch (kind) {
    case 'origin':
      return {
        label: 'Operator',
        body: `Inspect the active obligation at logical position ${serial}.`,
      }
    case 'progress':
      return { label: 'Progress', body: `Projection advanced through durable event ${serial}.` }
    case 'tool':
      return {
        label: 'Tool call',
        body: `repository.status completed with bounded summary ${serial}.`,
      }
    case 'result':
      return {
        label: 'Durable result',
        body: `The requested operation settled at cursor timeline:${index}.`,
      }
    case 'unknown':
      return { label: 'Unrecognized record', body: `kind=extension.preview; evidence=${serial}` }
  }
}

export class ScenarioTransport implements SignalboxTransport {
  readonly scenario: ScenarioDefinition

  constructor(id: ScenarioId) {
    this.scenario = scenarios.find((scenario) => scenario.id === id) ?? scenarios[0]
  }

  async readTimeline(request: WindowRequest): Promise<CursorWindow<TimelineItem>> {
    if (this.scenario.id === 'session-foundation') {
      const result = await sessionFoundationScenario(request.after, request.limit)
      const items = result.window.items.map((item, offset) => {
        const sequence = item.address.event_sequence
        const kind: TimelineKind = item.kind.includes('tool')
          ? 'tool'
          : item.kind === 'turn_completed'
            ? 'result'
            : item.kind === 'input_accepted'
              ? 'origin'
              : 'progress'
        return {
          id: `event-${sequence}`,
          cursor: `timeline:${sequence}`,
          turn: Math.max(Math.floor((Number(sequence) - 1) / 6) + 1, 1),
          kind,
          label: item.kind.replaceAll('_', ' '),
          body: `Durable header at stable address ${sequence}; detail is loaded separately.`,
          elapsed: `${(offset % 41) + 1}s`,
        }
      })
      return {
        items,
        nextCursor: result.window.continuation_after
          ? `timeline:${result.window.continuation_after.event_sequence}`
          : undefined,
        totalCount: Number(result.descriptor.sizes.item_count),
      }
    }
    const start = Math.min(parseCursor(request.after, 'timeline'), this.scenario.timelineTotal)
    const count = normalizedLimit({
      requested: request.limit,
      maximum: SCENARIO_TIMELINE_WINDOW_ITEMS,
    })
    const end = Math.min(start + count, this.scenario.timelineTotal)
    const items = Array.from({ length: end - start }, (_, offset) => {
      const index = start + offset
      const kind = timelineKind(index)
      const { label, body } = timelineCopy(kind, index)
      return {
        id: `event-${index}`,
        cursor: `timeline:${index}`,
        turn: Math.floor(index / 6) + 1,
        kind,
        label,
        body,
        elapsed: `${(index % 41) + 1}s`,
      }
    })
    return {
      items,
      nextCursor: end < this.scenario.timelineTotal ? `timeline:${end - 1}` : undefined,
      totalCount: this.scenario.timelineTotal,
    }
  }

  async readFleet(request: WindowRequest): Promise<CursorWindow<FleetRow>> {
    const start = Math.min(parseCursor(request.after, 'fleet'), this.scenario.tableTotal)
    const count = normalizedLimit({
      requested: request.limit,
      maximum: SCENARIO_FLEET_WINDOW_ITEMS,
    })
    const end = Math.min(start + count, this.scenario.tableTotal)
    const items = Array.from({ length: end - start }, (_, offset) => {
      const index = start + offset
      return {
        id: `obligation-${index}`,
        cursor: `fleet:${index}`,
        repository: `signalbox/worktree-${String(index + 1).padStart(4, '0')}`,
        state: fleetStateAt(index),
        purpose: index % 3 === 0 ? 'Review convergence' : 'Milestone implementation',
        age: `${(index % 58) + 1}m`,
      }
    })
    return {
      items,
      nextCursor: end < this.scenario.tableTotal ? `fleet:${end - 1}` : undefined,
      totalCount: this.scenario.tableTotal,
    }
  }
}
