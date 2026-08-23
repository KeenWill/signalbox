import { decodeWebContractBootstrap, type WebContractBootstrap } from './generated/web-contract.mjs'

const EXPECTED_BOOTSTRAP_LIMITS = {
  max_json_body_bytes: 65_536,
  max_ndjson_item_bytes: 65_536,
} as const
const BOOTSTRAP_RESPONSE_BYTES = 64 * 1024

export class ProductContractAdmissionError extends Error {
  constructor(cause: unknown) {
    super('web contract admission failed', { cause })
    this.name = 'ProductContractAdmissionError'
  }
}

export const productRoutes = [
  { id: 'attention', label: 'Attention', description: 'Actionable work and fleet state' },
  { id: 'sessions', label: 'Sessions', description: 'Conversation activity and history' },
  { id: 'search', label: 'Search', description: 'Global and session search' },
  { id: 'activity', label: 'Activity', description: 'Repository operations and ingestion' },
  { id: 'runners', label: 'Runners', description: 'Execution fleet' },
  { id: 'reviews', label: 'Reviews', description: 'Pull request convergence' },
  { id: 'imports', label: 'Imports', description: 'Imported conversations' },
  { id: 'usage', label: 'Usage', description: 'Tokens and cost' },
  { id: 'settings', label: 'Settings', description: 'Local workspace preferences' },
] as const

export type ProductRouteId = (typeof productRoutes)[number]['id']

export type ProductSurfaceState =
  | { kind: 'browser-local'; authority: 'browser preferences' }
  | { kind: 'server-backed'; owningTrack: string; facts: readonly string[] }
  | {
      kind: 'committed-unimplemented'
      owningTrack: string
      facts: readonly string[]
    }

export const productSurfaceStates: Record<ProductRouteId, ProductSurfaceState> = {
  attention: {
    kind: 'committed-unimplemented',
    owningTrack: '#992 attention projections',
    facts: ['prioritized attention reads'],
  },
  sessions: {
    kind: 'committed-unimplemented',
    owningTrack: '#991 session projections',
    facts: ['bounded session index reads', 'session creation and lifecycle operations'],
  },
  search: {
    kind: 'committed-unimplemented',
    owningTrack: '#994 search and usage reads',
    facts: ['cross-session search reads'],
  },
  activity: {
    kind: 'committed-unimplemented',
    owningTrack: '#995 discovery reads',
    facts: ['system activity reads'],
  },
  runners: {
    kind: 'committed-unimplemented',
    owningTrack: '#995 discovery reads',
    facts: ['runner discovery reads'],
  },
  reviews: {
    kind: 'committed-unimplemented',
    owningTrack: '#995 discovery reads',
    facts: ['review discovery reads'],
  },
  imports: {
    kind: 'server-backed',
    owningTrack: '#995 discovery reads',
    facts: ['bounded import catalog', 'descriptor and imported-entry windows', 'continuation'],
  },
  usage: {
    kind: 'committed-unimplemented',
    owningTrack: '#994 search and usage reads',
    facts: ['usage aggregation reads'],
  },
  settings: { kind: 'browser-local', authority: 'browser preferences' },
}

export interface ProductTransport {
  readBootstrap(signal?: AbortSignal): Promise<WebContractBootstrap>
}

const readBoundedJson = async (response: Response, maximumBytes: number): Promise<unknown> => {
  const contentLength = response.headers.get('Content-Length')
  if (contentLength !== null) {
    const declaredLength = Number(contentLength)
    if (Number.isFinite(declaredLength) && declaredLength > maximumBytes) {
      throw new Error('bootstrap response exceeds its byte ceiling')
    }
  }
  if (!response.body) throw new TypeError('bootstrap response has no body')
  const reader = response.body.getReader()
  const chunks: Uint8Array[] = []
  let receivedBytes = 0
  try {
    while (true) {
      const { done, value } = await reader.read()
      if (done) break
      receivedBytes += value.byteLength
      if (receivedBytes > maximumBytes) {
        await reader.cancel()
        throw new Error('bootstrap response exceeds its byte ceiling')
      }
      chunks.push(value)
    }
  } finally {
    reader.releaseLock()
  }
  const bytes = new Uint8Array(receivedBytes)
  let offset = 0
  for (const chunk of chunks) {
    bytes.set(chunk, offset)
    offset += chunk.byteLength
  }
  return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes))
}

export class SameOriginProductTransport implements ProductTransport {
  async readBootstrap(signal?: AbortSignal): Promise<WebContractBootstrap> {
    const response = await fetch('/api/bootstrap', {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) throw new Error(`bootstrap request failed with status ${response.status}`)
    try {
      const bootstrap = decodeWebContractBootstrap(
        await readBoundedJson(response, BOOTSTRAP_RESPONSE_BYTES),
      )
      if (Object.values(bootstrap.capabilities).some((enabled) => !enabled)) {
        throw new Error('incompatible web contract capabilities')
      }
      if (
        bootstrap.limits.max_json_body_bytes !== EXPECTED_BOOTSTRAP_LIMITS.max_json_body_bytes ||
        bootstrap.limits.max_ndjson_item_bytes !== EXPECTED_BOOTSTRAP_LIMITS.max_ndjson_item_bytes
      ) {
        throw new Error('incompatible web contract limits')
      }
      return bootstrap
    } catch (error) {
      throw new ProductContractAdmissionError(error)
    }
  }
}

export const productTransport = new SameOriginProductTransport()
