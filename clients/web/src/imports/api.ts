import {
  decodeWebApiErrorResponse,
  decodeWebContractBootstrap,
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
    knownLatestPosition?: number,
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

export class ImportListCorrelationError extends Error {
  constructor() {
    super('import catalog page does not correlate with its request')
    this.name = 'ImportListCorrelationError'
  }
}

const correlateListPage = (
  request: WebImportListRequest,
  page: WebImportListPage,
  searchCorrelation?: string,
): WebImportListPage => {
  if ((page.search_correlation ?? undefined) !== searchCorrelation) {
    throw new ImportListCorrelationError()
  }
  if (searchCorrelation !== undefined && !page.exact_source_session_id_sha256) {
    throw new ImportListCorrelationError()
  }
  let previous = request.after ?? undefined
  for (const item of page.items) {
    if (
      (previous !== undefined && item.imported_conversation_id <= previous) ||
      (request.format !== undefined && request.format !== null && item.format !== request.format)
    ) {
      throw new ImportListCorrelationError()
    }
    if (request.source_session_id !== undefined && request.source_session_id !== null) {
      const evidence = item.source_session_id
      if (
        evidence === undefined ||
        evidence === null ||
        (evidence.completeness === 'complete'
          ? evidence.leading_text !== request.source_session_id
          : !request.source_session_id.startsWith(evidence.leading_text))
      ) {
        throw new ImportListCorrelationError()
      }
      if (item.source_session_id_sha256 !== page.exact_source_session_id_sha256) {
        throw new ImportListCorrelationError()
      }
    }
    previous = item.imported_conversation_id
  }
  if (
    page.next_cursor !== undefined &&
    page.next_cursor !== null &&
    (page.items.length === 0 || page.next_cursor !== previous)
  ) {
    throw new ImportListCorrelationError()
  }
  return page
}

const DEFAULT_IMPORT_WINDOW_RADIUS = 25

const correlateEntryWindow = (
  importedConversationId: string,
  request: WebImportEntryWindowRequest,
  window: WebImportEntryWindow,
  knownLatestPosition?: number,
): WebImportEntryWindow => {
  const normalizedAnchor = request.anchor ?? 'first'
  const requestedBefore = request.before ?? DEFAULT_IMPORT_WINDOW_RADIUS
  const requestedAfter = request.after ?? DEFAULT_IMPORT_WINDOW_RADIUS
  const expectedAnchor =
    normalizedAnchor === 'first'
      ? 1
      : normalizedAnchor === 'latest'
        ? knownLatestPosition
        : request.position
  const positionsCorrelate = window.items.every(
    (entry, index) =>
      entry.frontier.imported_conversation_id === importedConversationId &&
      entry.frontier.position === window.first_position + index &&
      (entry.content_kind === 'text') === (entry.text !== undefined && entry.text !== null),
  )
  if (
    expectedAnchor === undefined ||
    window.items.length === 0 ||
    window.anchor_position !== expectedAnchor ||
    window.first_position > window.anchor_position ||
    window.last_position < window.anchor_position ||
    window.anchor_position - window.first_position > requestedBefore ||
    window.last_position - window.anchor_position > requestedAfter ||
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

export const validateWebContractBootstrap = async (): Promise<void> => {
  const response = await fetch('/api/bootstrap')
  await decodeResponse(response, decodeWebContractBootstrap)
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
  private bootstrapValidationPromise: Promise<void> | undefined

  constructor(private readonly bootstrapValidation = validateWebContractBootstrap) {}

  private validateBootstrap(): Promise<void> {
    this.bootstrapValidationPromise ??= this.bootstrapValidation().catch((error: unknown) => {
      this.bootstrapValidationPromise = undefined
      throw error
    })
    return this.bootstrapValidationPromise
  }

  async list(request: WebImportListRequest, signal?: AbortSignal): Promise<WebImportListPage> {
    await this.validateBootstrap()
    if (request.source_session_id !== undefined && request.source_session_id !== null) {
      const { source_session_id: sourceSessionId, ...catalogRequest } = request
      const searchCorrelation = crypto.randomUUID()
      const response = await fetch(
        `/api/imports/searches${queryString({
          ...catalogRequest,
          search_correlation: searchCorrelation,
        })}`,
        {
          method: 'POST',
          headers: { 'Content-Type': 'text/plain; charset=utf-8' },
          body: sourceSessionId,
          signal,
        },
      )
      return correlateListPage(
        request,
        await decodeResponse(response, decodeWebImportListPage),
        searchCorrelation,
      )
    }
    const response = await fetch(`/api/imports/${queryString(request)}`, { signal })
    return correlateListPage(request, await decodeResponse(response, decodeWebImportListPage))
  }

  async descriptor(
    importedConversationId: string,
    signal?: AbortSignal,
  ): Promise<WebImportDescriptor> {
    await this.validateBootstrap()
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
    knownLatestPosition?: number,
  ): Promise<WebImportEntryWindow> {
    await this.validateBootstrap()
    const response = await fetch(
      `/api/imports/${encodeURIComponent(importedConversationId)}/entries${queryString(request)}`,
      { signal },
    )
    const window = await decodeResponse(response, decodeWebImportEntryWindow)
    return correlateEntryWindow(importedConversationId, request, window, knownLatestPosition)
  }

  async continueImport(
    importedConversationId: string,
    request: WebImportContinuationRequest,
  ): Promise<WebImportContinuationResponse> {
    await this.validateBootstrap()
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
