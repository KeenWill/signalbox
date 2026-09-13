import {
  decodeWebApiErrorResponse,
  decodeWebSessionTitleRequest,
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

export async function renameSession(sessionId: string, request: WebSessionTitleRequest) {
  const controller = new AbortController()
  const deadline = setTimeout(() => controller.abort(), METADATA_PATCH_DEADLINE_MS)
  try {
    const response = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/metadata`, {
      method: 'PATCH',
      credentials: 'same-origin',
      headers: { 'content-type': 'application/json', accept: 'application/json' },
      body: JSON.stringify(decodeWebSessionTitleRequest(request)),
      signal: controller.signal,
    })
    if (response.status === 204) return
    if (!response.ok) {
      throw new ProductRequestError(
        response.status,
        decodeWebApiErrorResponse(await readBoundedJson(response)),
      )
    }
    throw new Error('Rename was not acknowledged. Retry to confirm the same title.')
  } catch (failure) {
    if (controller.signal.aborted) {
      throw new Error('Rename timed out. Retry to confirm the same title.', { cause: failure })
    }
    throw failure
  } finally {
    clearTimeout(deadline)
  }
}
