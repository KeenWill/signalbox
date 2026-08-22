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

export const MAX_PRODUCT_JSON_BYTES = 65_536

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
    const result = await reader.read()
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
  return JSON.parse(new TextDecoder().decode(bytes))
}

const request = async (input: RequestInfo | URL, init: RequestInit): Promise<Response> => {
  try {
    return await fetch(input, init)
  } catch (error) {
    throw new ProductTransportError(error)
  }
}

export class SameOriginProductTransport implements ProductTransport {
  async readBootstrap(signal?: AbortSignal): Promise<WebContractBootstrap> {
    const response = await request('/api/bootstrap', {
      headers: { accept: 'application/json' },
      credentials: 'same-origin',
      signal,
    })
    if (!response.ok) throw new Error(`bootstrap request failed with status ${response.status}`)
    return decodeWebContractBootstrap(await readBoundedJson(response))
  }

  async readBlobDescriptor(
    input: BlobDescriptorInput,
    signal?: AbortSignal,
  ): Promise<WebBlobDescriptor> {
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
    return decodeWebBlobDescriptor(payload)
  }
}

export const productTransport = new SameOriginProductTransport()
