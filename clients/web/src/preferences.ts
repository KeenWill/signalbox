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

export const createDefaultBrowserPreferences = (): BrowserPreferences => ({
  ...defaultBrowserPreferences,
  paneSizes: { ...defaultBrowserPreferences.paneSizes },
  lastLogicalPositions: {},
  keyOverrides: {},
})

export const BROWSER_PREFERENCES_KEY = 'signalbox.web.preferences.v1'
export const MAX_SAVED_LOGICAL_POSITIONS = 128
export const MAX_KEY_OVERRIDES = 64
export const MAX_LOGICAL_POSITION_KEY_BYTES = 512
export const MAX_LOGICAL_POSITION_VALUE_BYTES = 4_096

const utf8ByteLength = (value: string) => new TextEncoder().encode(value).byteLength

export const isBoundedLogicalPosition = (sessionId: string, position: string): boolean =>
  utf8ByteLength(sessionId) <= MAX_LOGICAL_POSITION_KEY_BYTES &&
  utf8ByteLength(position) <= MAX_LOGICAL_POSITION_VALUE_BYTES

const exactKeys = (value: Record<string, unknown>, expected: readonly string[]) =>
  Object.keys(value).length === expected.length &&
  expected.every((key) => Object.hasOwn(value, key))

const oneOf = <T extends string>(value: unknown, allowed: readonly T[], path: string): T => {
  if (typeof value !== 'string' || !allowed.includes(value as T)) {
    throw new TypeError(`${path} must be one of ${allowed.join(', ')}`)
  }
  return value as T
}

const boundedNumber = (value: unknown, minimum: number, maximum: number, path: string) => {
  if (typeof value !== 'number' || !Number.isFinite(value)) {
    throw new TypeError(`${path} must be a finite number`)
  }
  return Math.min(Math.max(Math.round(value), minimum), maximum)
}

const boundedRecord = (
  value: unknown,
  maximum: number,
  path: string,
  entryIsValid: (key: string, value: string) => boolean = () => true,
): Record<string, string> => {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new TypeError(`${path} must be an object`)
  }
  const entries = Object.entries(value)
  if (entries.some(([, entry]) => typeof entry !== 'string')) {
    throw new TypeError(`${path} values must be strings`)
  }
  if (entries.some(([key, entry]) => !entryIsValid(key, entry as string))) {
    throw new TypeError(`${path} keys or values exceed their byte limits`)
  }
  return Object.fromEntries(entries.slice(-maximum) as [string, string][])
}

export const decodeBrowserPreferences = (value: unknown): BrowserPreferences => {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new TypeError('preferences must be an object')
  }
  const candidate = value as Record<string, unknown>
  if (
    !exactKeys(candidate, [
      'layout',
      'density',
      'detail',
      'theme',
      'paneSizes',
      'remoteMedia',
      'lastLogicalPositions',
      'keyOverrides',
    ])
  ) {
    throw new TypeError('preferences must match the current exact schema')
  }
  if (
    candidate.paneSizes === null ||
    typeof candidate.paneSizes !== 'object' ||
    Array.isArray(candidate.paneSizes)
  ) {
    throw new TypeError('preferences.paneSizes must be an object')
  }
  const panes = candidate.paneSizes as Record<string, unknown>
  if (!exactKeys(panes, ['navigation', 'inspector'])) {
    throw new TypeError('preferences.paneSizes must match the current exact schema')
  }
  return {
    layout: oneOf(candidate.layout, ['focus', 'workbench'], 'preferences.layout'),
    density: oneOf(candidate.density, ['compact', 'comfortable'], 'preferences.density'),
    detail: oneOf(candidate.detail, ['full', 'condensed', 'results'], 'preferences.detail'),
    theme: oneOf(candidate.theme, ['light', 'dark'], 'preferences.theme'),
    paneSizes: {
      navigation: boundedNumber(panes.navigation, 160, 360, 'preferences.paneSizes.navigation'),
      inspector: boundedNumber(panes.inspector, 200, 480, 'preferences.paneSizes.inspector'),
    },
    remoteMedia: oneOf(candidate.remoteMedia, ['ask', 'block', 'allow'], 'preferences.remoteMedia'),
    lastLogicalPositions: boundedRecord(
      candidate.lastLogicalPositions,
      MAX_SAVED_LOGICAL_POSITIONS,
      'preferences.lastLogicalPositions',
      isBoundedLogicalPosition,
    ),
    keyOverrides: boundedRecord(
      candidate.keyOverrides,
      MAX_KEY_OVERRIDES,
      'preferences.keyOverrides',
    ),
  }
}

export const loadBrowserPreferences = (): BrowserPreferences => {
  try {
    const storage = globalThis.localStorage
    if (storage === undefined) return createDefaultBrowserPreferences()
    const stored = storage.getItem(BROWSER_PREFERENCES_KEY)
    if (stored === null) return createDefaultBrowserPreferences()
    return decodeBrowserPreferences(JSON.parse(stored))
  } catch {
    return createDefaultBrowserPreferences()
  }
}

export const saveBrowserPreferences = (preferences: BrowserPreferences): void => {
  try {
    const storage = globalThis.localStorage
    if (storage === undefined) return
    storage.setItem(BROWSER_PREFERENCES_KEY, JSON.stringify(preferences))
  } catch {
    // Browser persistence is optional; the active Redux state remains authoritative.
  }
}
