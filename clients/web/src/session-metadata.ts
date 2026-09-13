import {
  decodeWebApiErrorResponse,
  decodeWebSessionTitleRequest,
  type WebSessionTitleRequest,
} from './generated/web-contract.mjs'
import { ProductRequestError, readBoundedJson } from './product'

// Match the deadline for submitting session input.
const METADATA_PATCH_DEADLINE_MS = 30_000

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
