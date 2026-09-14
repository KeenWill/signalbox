import {
  decodeWebApiErrorResponse,
  decodeWebSessionTitleRequest,
  decodeWebSessionTitleSuggestion,
  type WebSessionCatalogSnapshot,
  type WebSessionTitleRequest,
} from './generated/web-contract.mjs'
import { ProductRequestError, readBoundedJson } from './product'

// Match the deadline for submitting session input.
const METADATA_PATCH_DEADLINE_MS = 30_000

export function createRenameCommandId() {
  // UUID v4 uses 16 random bytes with the version and variant bits fixed.
  const bytes = crypto.getRandomValues(new Uint8Array(16))
  const hex = Array.from(bytes, (byte, index) => {
    const value = index === 6 ? (byte & 0x0f) | 0x40 : index === 8 ? (byte & 0x3f) | 0x80 : byte
    return value.toString(16).padStart(2, '0')
  }).join('')
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`
}

const retainedRenames = new Map<
  string,
  { request: WebSessionTitleRequest; acknowledged: boolean }
>()

export const renameNeedsReadback = (sessionId: string) =>
  retainedRenames.get(sessionId)?.acknowledged === true

export async function readRenameCatalog(read: () => Promise<WebSessionCatalogSnapshot>) {
  const pending = new Map(retainedRenames)
  const snapshot = await read()
  for (const row of snapshot.summaries) {
    const intent = pending.get(row.session_id)
    if (intent?.acknowledged && retainedRenames.get(row.session_id) === intent) {
      retainedRenames.delete(row.session_id)
    }
  }
  return snapshot
}

export const retainedRename = (sessionId: string) => retainedRenames.get(sessionId)?.request ?? null

export async function renameSession(sessionId: string, request: WebSessionTitleRequest) {
  const retained = retainedRename(sessionId)
  if (
    retained &&
    (retained.command_id !== request.command_id || retained.title !== request.title)
  ) {
    throw new Error('Retry the unconfirmed rename before changing its title.')
  }
  const body = JSON.stringify(decodeWebSessionTitleRequest(request))
  if (!retained) retainedRenames.set(sessionId, { request, acknowledged: false })
  const controller = new AbortController()
  const deadline = setTimeout(() => controller.abort(), METADATA_PATCH_DEADLINE_MS)
  try {
    const response = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/metadata`, {
      method: 'PATCH',
      credentials: 'same-origin',
      headers: { 'content-type': 'application/json', accept: 'application/json' },
      body,
      signal: controller.signal,
    })
    if (response.status === 204) {
      retainedRenames.set(sessionId, { request, acknowledged: true })
      return
    }
    if (!response.ok) {
      throw new ProductRequestError(
        response.status,
        decodeWebApiErrorResponse(await readBoundedJson(response)),
      )
    }
    throw new Error('Rename was not acknowledged. Retry to confirm the same title.')
  } catch (failure) {
    if (failure instanceof ProductRequestError && [400, 404, 409, 413].includes(failure.status)) {
      retainedRenames.delete(sessionId)
    }
    if (controller.signal.aborted) {
      throw new Error('Rename timed out. Retry to confirm the same title.', { cause: failure })
    }
    throw failure
  } finally {
    clearTimeout(deadline)
  }
}

export async function suggestSessionTitle(sessionId: string) {
  const response = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/title/suggest`, {
    method: 'POST',
    credentials: 'same-origin',
    headers: { 'content-type': 'application/json', accept: 'application/json' },
    body: '{}',
  })
  const body = await readBoundedJson(response)
  if (!response.ok) {
    throw new ProductRequestError(response.status, decodeWebApiErrorResponse(body))
  }
  try {
    return decodeWebSessionTitleSuggestion(body)
  } catch (failure) {
    throw new Error('The suggested name could not be read. Try again.', { cause: failure })
  }
}
