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

export const productSurfaceCacheLabel = (surface: ProductRouteId): string | null => {
  switch (productSurfaceStates[surface].kind) {
    case 'browser-local':
      return 'Local settings'
    case 'server-backed':
      return 'Bounded query'
    case 'committed-unimplemented':
      return null
  }
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

export class ProductTransportError extends Error {
  constructor(cause: unknown) {
    super('The Signalbox daemon could not be reached.', { cause })
    this.name = 'ProductTransportError'
  }
}

export class ProductContractError extends Error {
  constructor(cause: unknown) {
    super('The bootstrap response did not match the generated web contract.', { cause })
    this.name = 'ProductContractError'
  }
}

export class ProductInputError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'ProductInputError'
  }
}

export const MAX_PRODUCT_JSON_BYTES = 65_536
export const MAX_NDJSON_ITEM_BYTES = 65_536
export const MAX_DECLARED_MEDIA_TYPE_BYTES = 255
export const MAX_DISPLAY_FILENAME_BYTES = 1_024

const utf8Length = (value: string): number => new TextEncoder().encode(value).byteLength

const validateBlobDescriptorInput = (input: BlobDescriptorInput): void => {
  if (utf8Length(input.mediaType) > MAX_DECLARED_MEDIA_TYPE_BYTES) {
    throw new ProductInputError('Descriptor media type exceeded the 255-byte limit.')
  }
  if (
    input.displayFilename !== undefined &&
    utf8Length(input.displayFilename) > MAX_DISPLAY_FILENAME_BYTES
  ) {
    throw new ProductInputError('Descriptor display filename exceeded the 1024-byte limit.')
  }
}

const readBoundedJson = async (response: Response): Promise<unknown> => {
  const declaredLength = Number(response.headers.get('content-length'))
  if (Number.isFinite(declaredLength) && declaredLength > MAX_PRODUCT_JSON_BYTES) {
    throw new Error('response exceeded the product JSON byte limit')
  }

  if (!response.body) return JSON.parse(await response.text())

  const reader = response.body.getReader()
  const chunks: Uint8Array[] = []
  let received = 0
  while (true) {
    let result: ReadableStreamReadResult<Uint8Array>
    try {
      result = await reader.read()
    } catch (error) {
      throw new ProductTransportError(error)
    }
    if (result.done) break
    received += result.value.byteLength
    if (received > MAX_PRODUCT_JSON_BYTES) {
      await reader.cancel()
      throw new Error('response exceeded the product JSON byte limit')
    }
    chunks.push(result.value)
  }

  const bytes = new Uint8Array(received)
  let offset = 0
  for (const chunk of chunks) {
    bytes.set(chunk, offset)
    offset += chunk.byteLength
  }
  return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes))
}

const request = async (input: RequestInfo | URL, init: RequestInit): Promise<Response> => {
  try {
    return await fetch(input, init)
  } catch (error) {
    throw new ProductTransportError(error)
  }
}

const validateCurrentBootstrap = (bootstrap: WebContractBootstrap): WebContractBootstrap => {
  if (
    bootstrap.contract.name !== 'signalbox.web-http' ||
    bootstrap.contract.version !== '2' ||
    bootstrap.limits.max_json_body_bytes !== MAX_PRODUCT_JSON_BYTES ||
    bootstrap.limits.max_ndjson_item_bytes !== MAX_NDJSON_ITEM_BYTES ||
    !bootstrap.capabilities.bounded_json ||
    !bootstrap.capabilities.same_origin_json_mutations ||
    !bootstrap.capabilities.ndjson_streaming ||
    (bootstrap.capabilities.blob_derivations && !bootstrap.capabilities.immutable_blob_content) ||
    (bootstrap.capabilities.image_derivatives && !bootstrap.capabilities.blob_derivations)
  ) {
    throw new Error('bootstrap contradicted the fixed signalbox.web-http v2 contract')
  }
  return bootstrap
}

export class SameOriginProductTransport implements ProductTransport {
  async readBootstrap(signal?: AbortSignal): Promise<WebContractBootstrap> {
    const response = await request('/api/bootstrap', {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) throw new Error(`bootstrap request failed with status ${response.status}`)
    try {
      return validateCurrentBootstrap(decodeWebContractBootstrap(await readBoundedJson(response)))
    } catch (error) {
      if (error instanceof ProductTransportError) throw error
      throw new ProductContractError(error)
    }
  }

  async readBlobDescriptor(
    input: BlobDescriptorInput,
    signal?: AbortSignal,
  ): Promise<WebBlobDescriptor> {
    validateBlobDescriptorInput(input)
    const query = new URLSearchParams({ media_type: input.mediaType })
    if (input.displayFilename) query.set('display_filename', input.displayFilename)
    const response = await request(
      `/api/blobs/${encodeURIComponent(input.digest)}/descriptor?${query.toString()}`,
      {
        headers: { accept: 'application/json' },
        credentials: 'same-origin',
        signal,
      },
    )
    const payload = await readBoundedJson(response)
    if (!response.ok) {
      throw new ProductRequestError(response.status, decodeWebApiErrorResponse(payload))
    }
    const descriptor = decodeWebBlobDescriptor(payload)
    if (descriptor.digest !== input.digest) {
      throw new Error('descriptor digest did not match the requested blob identity')
    }
    if (descriptor.declared_media_type !== input.mediaType) {
      throw new Error('descriptor media type did not match the requested blob use')
    }
    const expectedFilenames = input.displayFilename ? [input.displayFilename] : []
    if (
      descriptor.display_filename.length !== expectedFilenames.length ||
      descriptor.display_filename.some((filename, index) => filename !== expectedFilenames[index])
    ) {
      throw new Error('descriptor filename did not match the requested blob use')
    }
    return descriptor
  }
}

export const productTransport = new SameOriginProductTransport()
