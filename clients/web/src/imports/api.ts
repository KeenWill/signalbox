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

export class ImportResponseTooLargeError extends Error {
  constructor() {
    super('import API response exceeds its byte ceiling')
    this.name = 'ImportResponseTooLargeError'
  }
}

const correlateListPage = (
  request: WebImportListRequest,
  page: WebImportListPage,
  searchCorrelation?: string,
  exactSourceSessionDigest?: string,
): WebImportListPage => {
  const requestedLimit = request.limit ?? DEFAULT_IMPORT_LIST_ITEMS
  if ((page.search_correlation ?? undefined) !== searchCorrelation) {
    throw new ImportListCorrelationError()
  }
  if (
    searchCorrelation !== undefined &&
    page.exact_source_session_id_sha256 !== exactSourceSessionDigest
  ) {
    throw new ImportListCorrelationError()
  }
  if (
    page.items.length > requestedLimit ||
    (page.next_cursor !== undefined &&
      page.next_cursor !== null &&
      page.items.length !== requestedLimit)
  ) {
    throw new ImportListCorrelationError()
  }
  let previous = request.after ?? undefined
  for (const item of page.items) {
    const sourceSessionEvidence = item.source_session_id
    if (
      !isCanonicalUuid(item.imported_conversation_id) ||
      (previous !== undefined && item.imported_conversation_id <= previous) ||
      (request.format !== undefined && request.format !== null && item.format !== request.format) ||
      (sourceSessionEvidence !== undefined &&
        sourceSessionEvidence !== null &&
        new TextEncoder().encode(sourceSessionEvidence.leading_text).byteLength >
          MAX_IMPORT_TEXT_PREVIEW_BYTES)
    ) {
      throw new ImportListCorrelationError()
    }
    if (request.source_session_id !== undefined && request.source_session_id !== null) {
      const evidence = sourceSessionEvidence
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

const sha256 = async (value: string): Promise<string> => {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(value))
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, '0')).join('')
}

const DEFAULT_IMPORT_LIST_ITEMS = 50
const DEFAULT_IMPORT_WINDOW_RADIUS = 25
const BOOTSTRAP_RESPONSE_BYTES = 64 * 1024
const LIST_RESPONSE_BYTES = 1024 * 1024
const DESCRIPTOR_RESPONSE_BYTES = 128 * 1024
const ENTRY_WINDOW_RESPONSE_BYTES = 2 * 1024 * 1024
const CONTINUATION_RESPONSE_BYTES = 128 * 1024
const BOOTSTRAP_VALIDATION_TTL_MS = 30_000
const MAX_IMPORT_TEXT_PREVIEW_BYTES = 512
const CANONICAL_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/
const SHA256_HEX = /^[0-9a-f]{64}$/
const NIL_UUID = '00000000-0000-0000-0000-000000000000'

const isCanonicalUuid = (value: string): boolean => CANONICAL_UUID.test(value) && value !== NIL_UUID

const textPreviewIsBounded = (entry: WebImportEntryWindow['items'][number]): boolean => {
  if (entry.text?.kind !== 'attested') return true
  return (
    new TextEncoder().encode(entry.text.leading_text).byteLength <= MAX_IMPORT_TEXT_PREVIEW_BYTES
  )
}

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
  const expectedFirstPosition =
    expectedAnchor == null ? undefined : Math.max(1, expectedAnchor - requestedBefore)
  const expectedLastPosition =
    expectedAnchor == null || knownLatestPosition === undefined
      ? undefined
      : Math.min(knownLatestPosition, expectedAnchor + requestedAfter)
  const entryIdentities = new Set(window.items.map((entry) => entry.frontier.imported_entry_id))
  const positionsCorrelate = window.items.every(
    (entry, index) =>
      entry.frontier.imported_conversation_id === importedConversationId &&
      isCanonicalUuid(entry.frontier.imported_entry_id) &&
      entry.frontier.position > 0 &&
      entry.raw_record_position > 0 &&
      entry.record_entry_position > 0 &&
      entry.frontier.position === window.first_position + index &&
      (entry.content_kind === 'text') === (entry.text !== undefined && entry.text !== null) &&
      textPreviewIsBounded(entry),
  )
  if (
    expectedAnchor == null ||
    expectedFirstPosition === undefined ||
    expectedLastPosition === undefined ||
    knownLatestPosition === undefined ||
    window.items.length === 0 ||
    window.first_position <= 0 ||
    window.anchor_position !== expectedAnchor ||
    window.first_position !== expectedFirstPosition ||
    window.last_position !== expectedLastPosition ||
    window.first_position > window.anchor_position ||
    window.last_position < window.anchor_position ||
    window.anchor_position - window.first_position > requestedBefore ||
    window.last_position - window.anchor_position > requestedAfter ||
    window.last_position > knownLatestPosition ||
    window.last_position - window.first_position + 1 !== window.items.length ||
    entryIdentities.size !== window.items.length ||
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
  maximumBytes: number,
): Promise<Value> => {
  const contentLength = response.headers.get('Content-Length')
  if (contentLength !== null) {
    const declaredLength = Number(contentLength)
    if (Number.isFinite(declaredLength) && declaredLength > maximumBytes) {
      throw new ImportResponseTooLargeError()
    }
  }
  if (!response.body) throw new TypeError('import API response has no body')
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
        throw new ImportResponseTooLargeError()
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
  const value: unknown = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes))
  if (!response.ok) throw new ImportApiError(decodeWebApiErrorResponse(value))
  return decoder(value)
}

export const validateWebContractBootstrap = async (): Promise<void> => {
  const response = await fetch('/api/bootstrap')
  await decodeResponse(response, decodeWebContractBootstrap, BOOTSTRAP_RESPONSE_BYTES)
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
  private bootstrapValidatedAt: number | undefined

  constructor(
    private readonly bootstrapValidation = validateWebContractBootstrap,
    private readonly now = Date.now,
  ) {}

  private validateBootstrap(): Promise<void> {
    if (
      this.bootstrapValidatedAt !== undefined &&
      this.now() - this.bootstrapValidatedAt >= BOOTSTRAP_VALIDATION_TTL_MS
    ) {
      this.bootstrapValidationPromise = undefined
      this.bootstrapValidatedAt = undefined
    }
    this.bootstrapValidationPromise ??= this.bootstrapValidation()
      .then(() => {
        this.bootstrapValidatedAt = this.now()
      })
      .catch((error: unknown) => {
        this.bootstrapValidationPromise = undefined
        this.bootstrapValidatedAt = undefined
        throw error
      })
    return this.bootstrapValidationPromise
  }

  async list(request: WebImportListRequest, signal?: AbortSignal): Promise<WebImportListPage> {
    await this.validateBootstrap()
    if (request.source_session_id !== undefined && request.source_session_id !== null) {
      const { source_session_id: sourceSessionId, ...catalogRequest } = request
      const searchCorrelation = crypto.randomUUID()
      const exactSourceSessionDigest = await sha256(sourceSessionId)
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
        await decodeResponse(response, decodeWebImportListPage, LIST_RESPONSE_BYTES),
        searchCorrelation,
        exactSourceSessionDigest,
      )
    }
    const response = await fetch(`/api/imports/${queryString(request)}`, { signal })
    return correlateListPage(
      request,
      await decodeResponse(response, decodeWebImportListPage, LIST_RESPONSE_BYTES),
    )
  }

  async descriptor(
    importedConversationId: string,
    signal?: AbortSignal,
  ): Promise<WebImportDescriptor> {
    await this.validateBootstrap()
    const response = await fetch(`/api/imports/${encodeURIComponent(importedConversationId)}`, {
      signal,
    })
    const descriptor = await decodeResponse(
      response,
      decodeWebImportDescriptor,
      DESCRIPTOR_RESPONSE_BYTES,
    )
    if (
      descriptor.imported_conversation_id !== importedConversationId ||
      descriptor.timeline.first.imported_conversation_id !== importedConversationId ||
      descriptor.timeline.latest.imported_conversation_id !== importedConversationId ||
      descriptor.entry_count === 0 ||
      descriptor.timeline.first.position !== 1 ||
      descriptor.timeline.latest.position !== descriptor.entry_count ||
      !SHA256_HEX.test(descriptor.source.source_digest_sha256) ||
      (descriptor.source.source_session_id !== undefined &&
        descriptor.source.source_session_id !== null &&
        new TextEncoder().encode(descriptor.source.source_session_id.leading_text).byteLength >
          MAX_IMPORT_TEXT_PREVIEW_BYTES)
    ) {
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
    const window = await decodeResponse(
      response,
      decodeWebImportEntryWindow,
      ENTRY_WINDOW_RESPONSE_BYTES,
    )
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
    const receipt = await decodeResponse(
      response,
      decodeWebImportContinuationResponse,
      CONTINUATION_RESPONSE_BYTES,
    )
    if (
      !CANONICAL_UUID.test(receipt.session_id) ||
      receipt.session_id === '00000000-0000-0000-0000-000000000000' ||
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
