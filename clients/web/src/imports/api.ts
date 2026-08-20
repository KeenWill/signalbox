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
  list(request: WebImportListRequest): Promise<WebImportListPage>
  descriptor(importedConversationId: string): Promise<WebImportDescriptor>
  entries(
    importedConversationId: string,
    request: WebImportEntryWindowRequest,
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
  async list(request: WebImportListRequest): Promise<WebImportListPage> {
    const response = await fetch(`/api/imports/${queryString(request)}`)
    return decodeResponse(response, decodeWebImportListPage)
  }

  async descriptor(importedConversationId: string): Promise<WebImportDescriptor> {
    const response = await fetch(`/api/imports/${encodeURIComponent(importedConversationId)}`)
    return decodeResponse(response, decodeWebImportDescriptor)
  }

  async entries(
    importedConversationId: string,
    request: WebImportEntryWindowRequest,
  ): Promise<WebImportEntryWindow> {
    const response = await fetch(
      `/api/imports/${encodeURIComponent(importedConversationId)}/entries${queryString(request)}`,
    )
    return decodeResponse(response, decodeWebImportEntryWindow)
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
    return decodeResponse(response, decodeWebImportContinuationResponse)
  }
}
