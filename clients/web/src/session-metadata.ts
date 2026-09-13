import {
  decodeWebSessionTitleRequest,
  type WebSessionTitleRequest,
} from './generated/web-contract.mjs'

export async function renameSession(sessionId: string, request: WebSessionTitleRequest) {
  const response = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/metadata`, {
    method: 'PATCH',
    credentials: 'same-origin',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(decodeWebSessionTitleRequest(request)),
  })
  if (response.status !== 204) throw new Error(`Rename failed (${response.status}).`)
}
