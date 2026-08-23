import type { DensityMode, DetailMode, LayoutMode, ThemeMode } from './state'

export type RemoteMediaPolicy = 'ask' | 'block' | 'allow'

export interface BrowserPreferences {
  layout: LayoutMode
  density: DensityMode
  detail: DetailMode
  theme: ThemeMode
  paneSizes: { navigation: number; inspector: number }
  remoteMedia: RemoteMediaPolicy
  lastLogicalPositions: Record<string, string>
  keyOverrides: Record<string, string>
}

export const defaultBrowserPreferences: BrowserPreferences = {
  layout: 'workbench',
  density: 'compact',
  detail: 'condensed',
  theme: 'dark',
  paneSizes: { navigation: 218, inspector: 252 },
  remoteMedia: 'ask',
  lastLogicalPositions: {},
  keyOverrides: {},
}

export const BROWSER_PREFERENCES_KEY = 'signalbox.web.preferences.v1'
export const MAX_SAVED_LOGICAL_POSITIONS = 128
export const MAX_KEY_OVERRIDES = 64
// Hard safety ceiling: bounds each retained preference-record key and its encoding work.
export const MAX_PREFERENCE_RECORD_KEY_BYTES = 256
// Hard safety ceiling: bounds each retained preference-record value and its encoding work.
export const MAX_PREFERENCE_RECORD_VALUE_BYTES = 4_096
// Hard safety ceiling: bounds browser-storage reads, JSON parsing, and persisted preference bytes.
export const MAX_PREFERENCE_STORAGE_BYTES = 65_536

const utf8Bytes = (value: string) => new TextEncoder().encode(value).byteLength

const oneOf = <T extends string>(value: unknown, allowed: readonly T[], fallback: T): T =>
  typeof value === 'string' && allowed.includes(value as T) ? (value as T) : fallback

const boundedNumber = (value: unknown, fallback: number, minimum: number, maximum: number) =>
  typeof value === 'number' && Number.isFinite(value)
    ? Math.min(Math.max(Math.round(value), minimum), maximum)
    : fallback

const boundedRecord = (value: unknown, maximum: number): Record<string, string> => {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return {}
  const retained: [string, string][] = []
  let retainedBytes = 0
  const entries = Object.entries(value)
  for (let index = entries.length - 1; index >= 0 && retained.length < maximum; index -= 1) {
    const entry = entries[index]
    if (entry === undefined || typeof entry[1] !== 'string') continue
    const keyBytes = utf8Bytes(entry[0])
    const valueBytes = utf8Bytes(entry[1])
    if (
      keyBytes > MAX_PREFERENCE_RECORD_KEY_BYTES ||
      valueBytes > MAX_PREFERENCE_RECORD_VALUE_BYTES ||
      retainedBytes + keyBytes + valueBytes > MAX_PREFERENCE_STORAGE_BYTES
    ) {
      continue
    }
    retained.push([entry[0], entry[1]])
    retainedBytes += keyBytes + valueBytes
  }
  return Object.fromEntries(retained.reverse())
}

export const decodeBrowserPreferences = (value: unknown): BrowserPreferences => {
  const candidate =
    value !== null && typeof value === 'object' && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : {}
  const panes =
    candidate.paneSizes !== null && typeof candidate.paneSizes === 'object'
      ? (candidate.paneSizes as Record<string, unknown>)
      : {}
  return {
    layout: oneOf(candidate.layout, ['focus', 'workbench'], defaultBrowserPreferences.layout),
    density: oneOf(
      candidate.density,
      ['compact', 'comfortable'],
      defaultBrowserPreferences.density,
    ),
    detail: oneOf(
      candidate.detail,
      ['full', 'condensed', 'results'],
      defaultBrowserPreferences.detail,
    ),
    theme: oneOf(candidate.theme, ['light', 'dark'], defaultBrowserPreferences.theme),
    paneSizes: {
      navigation: boundedNumber(
        panes.navigation,
        defaultBrowserPreferences.paneSizes.navigation,
        160,
        360,
      ),
      inspector: boundedNumber(
        panes.inspector,
        defaultBrowserPreferences.paneSizes.inspector,
        200,
        480,
      ),
    },
    remoteMedia: oneOf(
      candidate.remoteMedia,
      ['ask', 'block', 'allow'],
      defaultBrowserPreferences.remoteMedia,
    ),
    lastLogicalPositions: boundedRecord(
      candidate.lastLogicalPositions,
      MAX_SAVED_LOGICAL_POSITIONS,
    ),
    keyOverrides: boundedRecord(candidate.keyOverrides, MAX_KEY_OVERRIDES),
  }
}

export const loadBrowserPreferences = (): BrowserPreferences => {
  try {
    if (typeof localStorage === 'undefined') return defaultBrowserPreferences
    const stored = localStorage.getItem(BROWSER_PREFERENCES_KEY)
    if (stored === null) return defaultBrowserPreferences
    if (utf8Bytes(stored) > MAX_PREFERENCE_STORAGE_BYTES) return defaultBrowserPreferences
    return decodeBrowserPreferences(JSON.parse(stored))
  } catch {
    return defaultBrowserPreferences
  }
}

export const saveBrowserPreferences = (preferences: BrowserPreferences): void => {
  try {
    if (typeof localStorage === 'undefined') return
    const stored = JSON.stringify(decodeBrowserPreferences(preferences))
    if (utf8Bytes(stored) > MAX_PREFERENCE_STORAGE_BYTES) return
    localStorage.setItem(BROWSER_PREFERENCES_KEY, stored)
  } catch {
    // The in-memory store remains authoritative when persistence is unavailable.
  }
}

export const applyStoredVisualPreferences = (): void => {
  const preferences = loadBrowserPreferences()
  document.documentElement.dataset.theme = preferences.theme
  document.documentElement.dataset.density = preferences.density
}
