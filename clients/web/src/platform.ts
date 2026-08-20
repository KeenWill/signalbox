export type ScenarioId =
  | 'streaming'
  | 'approval'
  | 'recovery'
  | 'large-timeline'
  | 'large-table'
  | 'huge-source'
  | 'blobs'
  | 'responsive'

export type TimelineKind = 'origin' | 'progress' | 'tool' | 'result' | 'unknown'

export interface ScenarioDefinition {
  id: ScenarioId
  title: string
  description: string
  connection: 'connected' | 'recovering'
  timelineTotal: number
  tableTotal: number
}

export interface TimelineItem {
  id: string
  cursor: string
  turn: number
  kind: TimelineKind
  label: string
  body: string
  elapsed: string
}

export interface FleetRow {
  id: string
  cursor: string
  repository: string
  state: 'active' | 'queued' | 'blocked' | 'settled'
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

export interface SignalboxTransport {
  readonly scenario: ScenarioDefinition
  readTimeline(request: WindowRequest): Promise<CursorWindow<TimelineItem>>
  readFleet(request: WindowRequest): Promise<CursorWindow<FleetRow>>
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
    id: 'blobs',
    title: 'Blob evidence',
    description: 'Artifact summaries avoid eager binary materialization.',
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
] as const satisfies readonly [ScenarioDefinition, ...ScenarioDefinition[]]

// Hard safety ceiling: scenario reads cannot allocate an entire logical timeline.
export const SCENARIO_TIMELINE_WINDOW_ITEMS = 360
// Hard safety ceiling: scenario reads cannot allocate an entire logical fleet table.
export const SCENARIO_FLEET_WINDOW_ITEMS = 480

const normalizedLimit = (limit: number, maximum: number): number => {
  if (!Number.isFinite(limit)) return 1
  return Math.min(Math.max(Math.trunc(limit), 1), maximum)
}

const parseCursor = (cursor: string | undefined, prefix: string): number => {
  if (!cursor?.startsWith(`${prefix}:`)) return 0
  const parsed = Number(cursor.slice(prefix.length + 1))
  return Number.isSafeInteger(parsed) && parsed >= 0 ? parsed + 1 : 0
}

const timelineKind = (index: number): TimelineKind => {
  if (index % 17 === 0) return 'unknown'
  if (index % 11 === 0) return 'result'
  if (index % 5 === 0) return 'tool'
  if (index % 3 === 0) return 'progress'
  return 'origin'
}

const timelineCopy = (kind: TimelineKind, index: number): [string, string] => {
  const serial = String(index + 1).padStart(5, '0')
  switch (kind) {
    case 'origin':
      return ['Operator', `Inspect the active obligation at logical position ${serial}.`]
    case 'progress':
      return ['Progress', `Projection advanced through durable event ${serial}.`]
    case 'tool':
      return ['Tool call', `repository.status completed with bounded summary ${serial}.`]
    case 'result':
      return ['Durable result', `The requested operation settled at cursor timeline:${index}.`]
    case 'unknown':
      return ['Unrecognized record', `kind=extension.preview; evidence=${serial}`]
  }
}

export class ScenarioTransport implements SignalboxTransport {
  readonly scenario: ScenarioDefinition

  constructor(id: ScenarioId) {
    this.scenario = scenarios.find((scenario) => scenario.id === id) ?? scenarios[0]
  }

  async readTimeline(request: WindowRequest): Promise<CursorWindow<TimelineItem>> {
    const start = Math.min(parseCursor(request.after, 'timeline'), this.scenario.timelineTotal)
    const count = normalizedLimit(request.limit, SCENARIO_TIMELINE_WINDOW_ITEMS)
    const end = Math.min(start + count, this.scenario.timelineTotal)
    const items = Array.from({ length: end - start }, (_, offset) => {
      const index = start + offset
      const kind = timelineKind(index)
      const [label, body] = timelineCopy(kind, index)
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
    const count = normalizedLimit(request.limit, SCENARIO_FLEET_WINDOW_ITEMS)
    const end = Math.min(start + count, this.scenario.tableTotal)
    const states: FleetRow['state'][] = ['active', 'queued', 'blocked', 'settled']
    const items = Array.from({ length: end - start }, (_, offset) => {
      const index = start + offset
      return {
        id: `obligation-${index}`,
        cursor: `fleet:${index}`,
        repository: `signalbox/worktree-${String(index + 1).padStart(4, '0')}`,
        state: states[index % states.length] ?? 'queued',
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
