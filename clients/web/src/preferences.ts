import type { DensityMode, DetailMode, LayoutMode, ThemeMode } from './state'

export interface BrowserPreferences {
  layout: LayoutMode
  density: DensityMode
  detail: DetailMode
  theme: ThemeMode
  paneSizes: { navigation: number; inspector: number }
  lastLogicalPositions: Record<string, string>
  keyOverrides: Record<string, string>
}

export const defaultBrowserPreferences: BrowserPreferences = {
  layout: 'workbench',
  density: 'compact',
  detail: 'condensed',
  theme: 'dark',
  paneSizes: { navigation: 218, inspector: 252 },
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

const oneOf = <T extends string>(value: unknown, allowed: readonly T[], fallback: T): T =>
  typeof value === 'string' && allowed.includes(value as T) ? (value as T) : fallback

const boundedNumber = (value: unknown, fallback: number, minimum: number, maximum: number) =>
  typeof value === 'number' && Number.isFinite(value)
    ? Math.min(Math.max(Math.round(value), minimum), maximum)
    : fallback

const boundedRecord = (value: unknown, maximum: number): Record<string, string> => {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return {}
  return Object.fromEntries(
    Object.entries(value)
      .filter((entry): entry is [string, string] => typeof entry[1] === 'string')
      .slice(-maximum),
  )
}

const MAX_U64 = (1n << 64n) - 1n
const canonicalSessionId = (value: string): boolean =>
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(value)
const canonicalTimelineAddress = (value: string): boolean => {
  if (!/^[1-9]\d*$/.test(value)) return false
  try {
    return BigInt(value) <= MAX_U64
  } catch {
    return false
  }
}
const boundedLogicalPositions = (value: unknown): Record<string, string> =>
  Object.fromEntries(
    Object.entries(boundedRecord(value, MAX_SAVED_LOGICAL_POSITIONS)).filter(
      ([sessionId, position]) =>
        canonicalSessionId(sessionId) && canonicalTimelineAddress(position),
    ),
  )

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
    lastLogicalPositions: boundedLogicalPositions(candidate.lastLogicalPositions),
    keyOverrides: boundedRecord(candidate.keyOverrides, MAX_KEY_OVERRIDES),
  }
}

export const loadBrowserPreferences = (): BrowserPreferences => {
  if (typeof localStorage === 'undefined') return createDefaultBrowserPreferences()
  try {
    const stored = localStorage.getItem(BROWSER_PREFERENCES_KEY)
    if (stored === null) return createDefaultBrowserPreferences()
    return decodeBrowserPreferences(JSON.parse(stored))
  } catch {
    return createDefaultBrowserPreferences()
  }
}

export const saveBrowserPreferences = (preferences: BrowserPreferences): void => {
  if (typeof localStorage === 'undefined') return
  try {
    localStorage.setItem(BROWSER_PREFERENCES_KEY, JSON.stringify(preferences))
  } catch {
    // Persistence is optional; the in-memory preference state remains valid.
  }
}
