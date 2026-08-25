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
export const MAX_KEY_OVERRIDE_KEY_BYTES = 512
export const MAX_KEY_OVERRIDE_VALUE_BYTES = 512
export const MAX_BROWSER_PREFERENCES_BYTES = 1_048_576
const MAX_U64 = (1n << 64n) - 1n

const isWithinUtf8ByteLimit = (value: string, limit: number): boolean => {
  let bytes = 0
  for (const character of value) {
    const codePoint = character.codePointAt(0) ?? 0
    bytes += codePoint <= 0x7f ? 1 : codePoint <= 0x7ff ? 2 : codePoint <= 0xffff ? 3 : 4
    if (bytes > limit) return false
  }
  return true
}

export const isBoundedLogicalPosition = (sessionId: string, position: string): boolean =>
  sessionId !== '__proto__' &&
  !/^(?:0|[1-9]\d*)$/.test(sessionId) &&
  isWithinUtf8ByteLimit(sessionId, MAX_LOGICAL_POSITION_KEY_BYTES) &&
  isWithinUtf8ByteLimit(position, MAX_LOGICAL_POSITION_VALUE_BYTES)

const isPersistedLogicalPosition = (sessionId: string, position: string): boolean =>
  isBoundedLogicalPosition(sessionId, position) &&
  /^[1-9]\d*$/.test(position) &&
  position.length <= 20 &&
  BigInt(position) <= MAX_U64

const isBoundedKeyOverride = (commandId: string, binding: string): boolean =>
  commandId !== '__proto__' &&
  !/^(?:0|[1-9]\d*)$/.test(commandId) &&
  isWithinUtf8ByteLimit(commandId, MAX_KEY_OVERRIDE_KEY_BYTES) &&
  isWithinUtf8ByteLimit(binding, MAX_KEY_OVERRIDE_VALUE_BYTES)

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
      isPersistedLogicalPosition,
    ),
    keyOverrides: boundedRecord(
      candidate.keyOverrides,
      MAX_KEY_OVERRIDES,
      'preferences.keyOverrides',
      isBoundedKeyOverride,
    ),
  }
}

export const loadBrowserPreferences = (): BrowserPreferences => {
  try {
    const storage = globalThis.localStorage
    if (storage === undefined) return createDefaultBrowserPreferences()
    const stored = storage.getItem(BROWSER_PREFERENCES_KEY)
    if (stored === null) return createDefaultBrowserPreferences()
    if (!isWithinUtf8ByteLimit(stored, MAX_BROWSER_PREFERENCES_BYTES)) {
      return createDefaultBrowserPreferences()
    }
    return decodeBrowserPreferences(JSON.parse(stored))
  } catch {
    return createDefaultBrowserPreferences()
  }
}

export const serializeBrowserPreferences = (preferences: BrowserPreferences): string | null => {
  const serialized = JSON.stringify(preferences)
  return isWithinUtf8ByteLimit(serialized, MAX_BROWSER_PREFERENCES_BYTES) ? serialized : null
}

export const saveBrowserPreferences = (preferences: BrowserPreferences): void => {
  try {
    const storage = globalThis.localStorage
    if (storage === undefined) return
    const serialized = serializeBrowserPreferences(preferences)
    if (serialized === null) return
    storage.setItem(BROWSER_PREFERENCES_KEY, serialized)
  } catch {
    // Browser persistence is optional; the active Redux state remains authoritative.
  }
}
