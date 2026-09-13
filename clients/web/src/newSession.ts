import {
  decodeWebApiErrorResponse,
  decodeWebCreateSessionRequest,
  decodeWebCreateSessionResponse,
  type WebCreateSessionRequest,
} from './generated/web-contract.mjs'

import { readBoundedJson } from './product'

const retainedCreationKey = 'signalbox.new-session'

export function readRetainedCreation(): WebCreateSessionRequest | null {
  const value = sessionStorage.getItem(retainedCreationKey)
  if (value === null) return null
  try {
    return decodeWebCreateSessionRequest(JSON.parse(value))
  } catch (error) {
    sessionStorage.removeItem(retainedCreationKey)
    throw error
  }
}

export function retainCreation(request: WebCreateSessionRequest) {
  sessionStorage.setItem(retainedCreationKey, JSON.stringify(request))
}

export function clearRetainedCreation() {
  sessionStorage.removeItem(retainedCreationKey)
}

export class SessionCreationRejected extends Error {}

export class HttpSessionCreationApi {
  constructor(private readonly request: typeof fetch = (input, init) => fetch(input, init)) {}

  async create(request: WebCreateSessionRequest) {
    const response = await this.request('/api/sessions', {
      method: 'POST',
      credentials: 'same-origin',
      headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
      body: JSON.stringify(request),
    })
    const value: unknown = await readBoundedJson(response)
    if (!response.ok) {
      const message = decodeWebApiErrorResponse(value).error.message
      if (response.status === 400 || response.status === 409)
        throw new SessionCreationRejected(message)
      throw new Error(message)
    }
    if (response.status !== 201) throw new Error('Session creation was not confirmed.')
    const result = decodeWebCreateSessionResponse(value)
    if (result.session_id !== result.summary.session_id)
      throw new Error('The created session does not match its summary.')
    return result
  }
}
