import type { WebTimelineTextExcerpt } from '../../generated/web-contract.mjs'
import { ARTIFACT_PREVIEW_CHARACTERS, ARTIFACT_PREVIEW_LINES } from '../artifacts/artifactTypes'

// The generated decodeWebSessionTimelineDetailPage contract bounds projected bodies to 64 KiB.
const MAX_TOOL_EXCERPT_BYTES = 64 * 1024

export type Fields = Record<string, unknown>

export const fields = (value: unknown): Fields =>
  value !== null && typeof value === 'object' && !Array.isArray(value) ? (value as Fields) : {}

export const textField = (value: unknown): string =>
  typeof value === 'string'
    ? value
    : typeof value === 'number' || typeof value === 'boolean'
      ? String(value)
      : ''

export const excerptFields = (excerpt?: WebTimelineTextExcerpt | null): Fields => {
  if (
    excerpt?.offset_bytes !== '0' ||
    excerpt.continuation != null ||
    excerpt.text.length > MAX_TOOL_EXCERPT_BYTES ||
    new TextEncoder().encode(excerpt.text).byteLength > MAX_TOOL_EXCERPT_BYTES
  )
    return {}
  try {
    return fields(
      JSON.parse(excerpt.text, (_key, value: unknown, context?: { source?: string }) => {
        // Keep the original excerpt whenever rendering the parsed number would change its token.
        if (typeof value === 'number' && String(value) !== context?.source)
          throw new RangeError('JSON number requires its original representation')
        return value
      }),
    )
  } catch {
    return {}
  }
}

export const previewText = (text: string) => {
  let characters = 0
  let prefixCharacters = 0
  let lineBreaks = 0
  let content = ''
  let previous = ''
  let stopped = false
  for (const character of text) {
    characters += 1
    if (stopped) continue
    if (character === '\r' || (character === '\n' && previous !== '\r')) lineBreaks += 1
    if (prefixCharacters === ARTIFACT_PREVIEW_CHARACTERS || lineBreaks === ARTIFACT_PREVIEW_LINES) {
      stopped = true
      continue
    }
    content += character
    prefixCharacters += 1
    previous = character
  }
  return { content, omittedCharacters: characters - prefixCharacters }
}

export const webLink = (value: unknown): string | undefined => {
  if (typeof value !== 'string') return undefined
  try {
    const url = new URL(value)
    return ['https:', 'http:'].includes(url.protocol) && !url.username && !url.password
      ? url.href
      : undefined
  } catch {
    return undefined
  }
}

export const fieldLabel = (key: string): string => {
  const words = key.replaceAll('_', ' ')
  return words.charAt(0).toUpperCase() + words.slice(1)
}

// Inverse of tools-web's quoted-attribute encoding, applied once before React text escaping.
const searchEntities: Readonly<Record<string, string>> = {
  '&amp;': '&',
  '&lt;': '<',
  '&gt;': '>',
  '&quot;': '"',
  '&#x27;': "'",
}
export const searchResultText = (value: unknown): string =>
  textField(value).replace(
    /&(amp|lt|gt|quot|#x27);/gu,
    (entity) => searchEntities[entity] ?? entity,
  )
