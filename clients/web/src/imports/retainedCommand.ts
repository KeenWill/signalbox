import {
  decodeWebImportContinuationRequest,
  type WebImportContinuationRequest,
} from '../generated/web-contract.mjs'

const RETAINED_COMMAND_STORAGE_PREFIX = 'signalbox.import-continuation.'
const MAX_RETAINED_COMMAND_BYTES = 4096

const storageKey = (scope: string): string => `${RETAINED_COMMAND_STORAGE_PREFIX}${scope}`

export const loadRetainedCommand = (scope: string): WebImportContinuationRequest | null => {
  const key = storageKey(scope)
  try {
    const encoded = window.sessionStorage.getItem(key)
    if (encoded === null) return null
    if (new TextEncoder().encode(encoded).length > MAX_RETAINED_COMMAND_BYTES) {
      window.sessionStorage.removeItem(key)
      return null
    }
    return decodeWebImportContinuationRequest(JSON.parse(encoded))
  } catch {
    try {
      window.sessionStorage.removeItem(key)
    } catch {
      // Route-local retention still applies when storage is unavailable.
    }
    return null
  }
}

export const storeRetainedCommand = (
  scope: string,
  command: WebImportContinuationRequest | null,
): boolean => {
  try {
    const key = storageKey(scope)
    if (command === null) {
      window.sessionStorage.removeItem(key)
      return true
    }
    const encoded = JSON.stringify(command)
    if (new TextEncoder().encode(encoded).length > MAX_RETAINED_COMMAND_BYTES) return false
    window.sessionStorage.setItem(key, encoded)
    return true
  } catch {
    return false
  }
}
