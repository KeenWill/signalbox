import {
  decodeWebApiErrorResponse,
  decodeWebImportContinuationResponse,
  decodeWebImportDescriptor,
  decodeWebImportEntryWindow,
  decodeWebImportListPage,
  type WebApiErrorResponse,
  type WebImportContinuationRequest,
  type WebImportContinuationResponse,
  type WebImportDescriptor,
  type WebImportEntryWindow,
  type WebImportEntryWindowRequest,
  type WebImportListPage,
  type WebImportListRequest,
} from '../generated/web-contract.mjs'

export interface ImportApi {
  list(request: WebImportListRequest, signal?: AbortSignal): Promise<WebImportListPage>
  descriptor(importedConversationId: string, signal?: AbortSignal): Promise<WebImportDescriptor>
  entries(
    importedConversationId: string,
    request: WebImportEntryWindowRequest,
    signal?: AbortSignal,
  ): Promise<WebImportEntryWindow>
  continueImport(
    importedConversationId: string,
    request: WebImportContinuationRequest,
  ): Promise<WebImportContinuationResponse>
}

export class ImportApiError extends Error {
  readonly detail: WebApiErrorResponse

  constructor(detail: WebApiErrorResponse) {
    super(detail.error.message)
    this.name = 'ImportApiError'
    this.detail = detail
  }
}

export class ImportReceiptCorrelationError extends Error {
  constructor() {
    super('continuation receipt does not correlate with its request')
    this.name = 'ImportReceiptCorrelationError'
  }
}

export class ImportWindowCorrelationError extends Error {
  constructor() {
    super('imported entry window does not correlate with its request')
    this.name = 'ImportWindowCorrelationError'
  }
}

export class ImportDescriptorCorrelationError extends Error {
  constructor() {
    super('import descriptor does not correlate with its request')
    this.name = 'ImportDescriptorCorrelationError'
  }
}

const correlateEntryWindow = (
  importedConversationId: string,
  request: WebImportEntryWindowRequest,
  window: WebImportEntryWindow,
): WebImportEntryWindow => {
  const expectedAnchor =
    request.anchor === 'first'
      ? 1
      : request.anchor === 'latest'
        ? window.last_position
        : request.position
  const positionsCorrelate = window.items.every(
    (entry, index) =>
      entry.frontier.imported_conversation_id === importedConversationId &&
      entry.frontier.position === window.first_position + index,
  )
  if (
    expectedAnchor === undefined ||
    window.items.length === 0 ||
    window.anchor_position !== expectedAnchor ||
    window.first_position > window.anchor_position ||
    window.last_position < window.anchor_position ||
    window.last_position - window.first_position + 1 !== window.items.length ||
    !positionsCorrelate ||
    !window.items.some((entry) => entry.frontier.position === window.anchor_position)
  ) {
    throw new ImportWindowCorrelationError()
  }
  return window
}

type Decoder<Value> = (value: unknown) => Value

const decodeResponse = async <Value>(
  response: Response,
  decoder: Decoder<Value>,
): Promise<Value> => {
  const value: unknown = await response.json()
  if (!response.ok) throw new ImportApiError(decodeWebApiErrorResponse(value))
  return decoder(value)
}

const queryString = (request: WebImportListRequest | WebImportEntryWindowRequest): string => {
  const query = new URLSearchParams()
  for (const [name, value] of Object.entries(request)) {
    if (value !== undefined && value !== null) query.set(name, String(value))
  }
  const encoded = query.toString()
  return encoded.length > 0 ? `?${encoded}` : ''
}

export class HttpImportApi implements ImportApi {
  async list(request: WebImportListRequest, signal?: AbortSignal): Promise<WebImportListPage> {
    if (request.source_session_id !== undefined && request.source_session_id !== null) {
      const response = await fetch('/api/imports/searches', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(request),
        signal,
      })
      return decodeResponse(response, decodeWebImportListPage)
    }
    const response = await fetch(`/api/imports/${queryString(request)}`, { signal })
    return decodeResponse(response, decodeWebImportListPage)
  }

  async descriptor(
    importedConversationId: string,
    signal?: AbortSignal,
  ): Promise<WebImportDescriptor> {
    const response = await fetch(`/api/imports/${encodeURIComponent(importedConversationId)}`, {
      signal,
    })
    const descriptor = await decodeResponse(response, decodeWebImportDescriptor)
    if (descriptor.imported_conversation_id !== importedConversationId) {
      throw new ImportDescriptorCorrelationError()
    }
    return descriptor
  }

  async entries(
    importedConversationId: string,
    request: WebImportEntryWindowRequest,
    signal?: AbortSignal,
  ): Promise<WebImportEntryWindow> {
    const response = await fetch(
      `/api/imports/${encodeURIComponent(importedConversationId)}/entries${queryString(request)}`,
      { signal },
    )
    const window = await decodeResponse(response, decodeWebImportEntryWindow)
    return correlateEntryWindow(importedConversationId, request, window)
  }

  async continueImport(
    importedConversationId: string,
    request: WebImportContinuationRequest,
  ): Promise<WebImportContinuationResponse> {
    const response = await fetch(
      `/api/imports/${encodeURIComponent(importedConversationId)}/continuations`,
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(request),
      },
    )
    const receipt = await decodeResponse(response, decodeWebImportContinuationResponse)
    if (
      receipt.command_id !== request.command_id ||
      receipt.relationship !== request.relationship ||
      receipt.frontier.imported_conversation_id !== request.frontier.imported_conversation_id ||
      receipt.frontier.imported_entry_id !== request.frontier.imported_entry_id ||
      receipt.frontier.position !== request.frontier.position
    ) {
      throw new ImportReceiptCorrelationError()
    }
    return receipt
  }
}
