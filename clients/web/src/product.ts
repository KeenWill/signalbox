import {
  decodeWebApiErrorResponse,
  decodeWebBlobDescriptor,
  decodeWebContractBootstrap,
  type WebApiErrorResponse,
  type WebBlobDescriptor,
  type WebContractBootstrap,
} from './generated/web-contract.mjs'

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
    kind: 'server-backed',
    owningTrack: '#991 session projections',
    facts: ['bounded session descriptors', 'stable-address timeline windows'],
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
    kind: 'committed-unimplemented',
    owningTrack: '#995 discovery reads',
    facts: ['import discovery reads'],
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
  readBlobDescriptor(input: BlobDescriptorInput, signal?: AbortSignal): Promise<WebBlobDescriptor>
}

export interface BlobDescriptorInput {
  digest: string
  mediaType: string
  displayFilename?: string
}

export class ProductRequestError extends Error {
  readonly status: number
  readonly response: WebApiErrorResponse

  constructor(status: number, response: WebApiErrorResponse) {
    super(response.error.message)
    this.name = 'ProductRequestError'
    this.status = status
    this.response = response
  }
}

export class SameOriginProductTransport implements ProductTransport {
  async readBootstrap(signal?: AbortSignal): Promise<WebContractBootstrap> {
    const response = await fetch('/api/bootstrap', {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) throw new Error(`bootstrap request failed with status ${response.status}`)
    return decodeWebContractBootstrap(await response.json())
  }

  async readBlobDescriptor(
    input: BlobDescriptorInput,
    signal?: AbortSignal,
  ): Promise<WebBlobDescriptor> {
    const query = new URLSearchParams({ media_type: input.mediaType })
    if (input.displayFilename) query.set('display_filename', input.displayFilename)
    const response = await fetch(
      `/api/blobs/${encodeURIComponent(input.digest)}/descriptor?${query.toString()}`,
      {
        headers: { accept: 'application/json' },
        credentials: 'same-origin',
        signal,
      },
    )
    const payload: unknown = await response.json()
    if (!response.ok) {
      throw new ProductRequestError(response.status, decodeWebApiErrorResponse(payload))
    }
    return decodeWebBlobDescriptor(payload)
  }
}

export const productTransport = new SameOriginProductTransport()
