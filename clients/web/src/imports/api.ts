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
import { readBoundedJson } from '../session-timeline/model'

const MAX_IMPORT_RESPONSE_BYTES = 1024 * 1024
const MAX_IMPORT_SOURCE_SESSION_BYTES = 512
const DEFAULT_IMPORT_WINDOW_RADIUS = 50
const utf8 = new TextEncoder()

const boundedUtf8Prefix = (value: string, maximumBytes: number): string => {
  const bytes = utf8.encode(value)
  if (bytes.byteLength <= maximumBytes) return value
  let end = maximumBytes
  while (end > 0 && ((bytes[end] ?? 0) & 0xc0) === 0x80) end -= 1
  return new TextDecoder('utf-8', { fatal: true }).decode(bytes.subarray(0, end))
}

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

export class ImportListCorrelationError extends Error {
  constructor() {
    super('imports catalog page does not correlate with its request')
    this.name = 'ImportListCorrelationError'
  }
}

const canonicalCatalogUuid = (value: string): string => {
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(value)) {
    throw new ImportListCorrelationError()
  }
  return value
}

const correlateListPage = (
  request: WebImportListRequest,
  page: WebImportListPage,
): WebImportListPage => {
  const requestedLimit = request.limit
  if (
    requestedLimit !== undefined &&
    requestedLimit !== null &&
    page.items.length > requestedLimit
  ) {
    throw new ImportListCorrelationError()
  }
  const requestedAfter = request.after
  let previousId =
    requestedAfter === undefined || requestedAfter === null
      ? undefined
      : canonicalCatalogUuid(requestedAfter)
  const requestedFormat = request.format
  const requestedSourceSessionId = request.source_session_id
  for (const item of page.items) {
    const itemId = canonicalCatalogUuid(item.imported_conversation_id)
    const sourceSessionCorrelates =
      requestedSourceSessionId === undefined ||
      requestedSourceSessionId === null ||
      (item.source_session_id !== undefined &&
        item.source_session_id !== null &&
        item.source_session_id.leading_text ===
          boundedUtf8Prefix(requestedSourceSessionId, MAX_IMPORT_SOURCE_SESSION_BYTES) &&
        item.source_session_id.completeness ===
          (utf8.encode(requestedSourceSessionId).byteLength > MAX_IMPORT_SOURCE_SESSION_BYTES
            ? 'truncated'
            : 'complete'))
    if (
      (previousId !== undefined && itemId <= previousId) ||
      (requestedFormat !== undefined &&
        requestedFormat !== null &&
        item.format !== requestedFormat) ||
      !sourceSessionCorrelates
    ) {
      throw new ImportListCorrelationError()
    }
    previousId = itemId
  }
  const nextCursor = page.next_cursor
  if (
    nextCursor !== null &&
    nextCursor !== undefined &&
    (page.items.length === 0 ||
      canonicalCatalogUuid(nextCursor) !==
        page.items[page.items.length - 1]?.imported_conversation_id)
  ) {
    throw new ImportListCorrelationError()
  }
  return page
}

const decimalPosition = (value: string): bigint => {
  if (!/^[1-9]\d{0,19}$/.test(value)) throw new ImportWindowCorrelationError()
  const parsed = BigInt(value)
  if (parsed > 18_446_744_073_709_551_615n) throw new ImportWindowCorrelationError()
  return parsed
}

const correlateEntryWindow = (
  importedConversationId: string,
  request: WebImportEntryWindowRequest,
  window: WebImportEntryWindow,
): WebImportEntryWindow => {
  const firstPosition = decimalPosition(window.first_position)
  const lastPosition = decimalPosition(window.last_position)
  const anchorPosition = decimalPosition(window.anchor_position)
  const expectedAnchor =
    request.anchor === undefined || request.anchor === null || request.anchor === 'first'
      ? 1n
      : request.anchor === 'latest'
        ? lastPosition
        : request.position === undefined || request.position === null
          ? undefined
          : decimalPosition(request.position)
  const entryIds = new Set<string>()
  const positionsCorrelate = window.items.every((entry, index) => {
    const entryId = entry.frontier.imported_entry_id
    if (entryIds.has(entryId)) return false
    entryIds.add(entryId)
    return (
      entry.frontier.imported_conversation_id === importedConversationId &&
      decimalPosition(entry.frontier.position) === firstPosition + BigInt(index)
    )
  })
  const requestedBefore = BigInt(request.before ?? DEFAULT_IMPORT_WINDOW_RADIUS)
  const requestedAfter = BigInt(request.after ?? DEFAULT_IMPORT_WINDOW_RADIUS)
  const returnedBefore = anchorPosition - firstPosition
  const returnedAfter = lastPosition - anchorPosition
  if (
    expectedAnchor === undefined ||
    window.items.length === 0 ||
    anchorPosition !== expectedAnchor ||
    firstPosition > anchorPosition ||
    lastPosition < anchorPosition ||
    lastPosition - firstPosition + 1n !== BigInt(window.items.length) ||
    (window.has_before && returnedBefore !== requestedBefore) ||
    (window.has_after && returnedAfter !== requestedAfter) ||
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
  const value = await readBoundedJson(response, MAX_IMPORT_RESPONSE_BYTES)
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
    const response = await fetch(`/api/imports/${queryString(request)}`, { signal })
    const page = await decodeResponse(response, decodeWebImportListPage)
    return correlateListPage(request, page)
  }

  async descriptor(
    importedConversationId: string,
    signal?: AbortSignal,
  ): Promise<WebImportDescriptor> {
    const response = await fetch(`/api/imports/${encodeURIComponent(importedConversationId)}`, {
      signal,
    })
    const descriptor = await decodeResponse(response, decodeWebImportDescriptor)
    if (
      descriptor.imported_conversation_id !== importedConversationId ||
      descriptor.timeline.first.imported_conversation_id !== importedConversationId ||
      descriptor.timeline.latest.imported_conversation_id !== importedConversationId ||
      descriptor.timeline.first.position !== '1' ||
      descriptor.timeline.latest.position !== descriptor.entry_count
    ) {
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
