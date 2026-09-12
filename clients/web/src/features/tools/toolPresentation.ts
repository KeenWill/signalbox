import type { WebTimelineTextExcerpt } from '../../generated/web-contract.mjs'
import { ARTIFACT_EXPANDED_CHARACTERS, boundArtifactText } from '../artifacts/artifactTypes'

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
    excerpt.text.length > ARTIFACT_EXPANDED_CHARACTERS
  )
    return {}
  try {
    return fields(JSON.parse(excerpt.text))
  } catch {
    return {}
  }
}

export const previewText = (text: string) => {
  let characters = 0
  for (const _character of text) characters += 1
  return boundArtifactText(text, characters, 'preview')
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
