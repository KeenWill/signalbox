import type { DensityMode, DetailMode, LayoutMode, ThemeMode } from './state'

export interface BrowserPreferences {
  layout: LayoutMode
  density: DensityMode
  detail: DetailMode
  theme: ThemeMode
  paneSizes: { navigation: number; inspector: number }
}

export const paneSizeBounds = {
  navigation: { minimum: 160, maximum: 360 },
  inspector: { minimum: 200, maximum: 480 },
} as const

export const defaultBrowserPreferences: BrowserPreferences = {
  layout: 'workbench',
  density: 'compact',
  detail: 'condensed',
  theme: 'dark',
  paneSizes: { navigation: 218, inspector: 252 },
}

export const createDefaultBrowserPreferences = (): BrowserPreferences => ({
  ...defaultBrowserPreferences,
  paneSizes: { ...defaultBrowserPreferences.paneSizes },
})

export const BROWSER_PREFERENCES_KEY = 'signalbox.web.preferences.v1'
export const MAX_BROWSER_PREFERENCES_BYTES = 16_384

const exactKeys = (value: Record<string, unknown>, expected: readonly string[]): boolean =>
  Object.keys(value).length === expected.length &&
  expected.every((key) => Object.hasOwn(value, key))

const utf8Length = (value: string): number => new TextEncoder().encode(value).byteLength

const oneOf = <T extends string>(value: unknown, allowed: readonly T[], path: string): T => {
  if (typeof value !== 'string' || !allowed.includes(value as T)) {
    throw new TypeError(`${path} must match the current schema`)
  }
  return value as T
}

const boundedNumber = (value: unknown, minimum: number, maximum: number, path: string) => {
  if (typeof value !== 'number' || !Number.isFinite(value)) {
    throw new TypeError(`${path} must be a finite number`)
  }
  return Math.min(Math.max(Math.round(value), minimum), maximum)
}

export const decodeBrowserPreferences = (value: unknown): BrowserPreferences => {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new TypeError('preferences must be an object')
  }
  const candidate = value as Record<string, unknown>
  if (!exactKeys(candidate, ['layout', 'density', 'detail', 'theme', 'paneSizes'])) {
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
      navigation: boundedNumber(
        panes.navigation,
        paneSizeBounds.navigation.minimum,
        paneSizeBounds.navigation.maximum,
        'preferences.paneSizes.navigation',
      ),
      inspector: boundedNumber(
        panes.inspector,
        paneSizeBounds.inspector.minimum,
        paneSizeBounds.inspector.maximum,
        'preferences.paneSizes.inspector',
      ),
    },
  }
}

export const loadBrowserPreferences = (): BrowserPreferences => {
  try {
    if (typeof localStorage === 'undefined') return createDefaultBrowserPreferences()
    const stored = localStorage.getItem(BROWSER_PREFERENCES_KEY)
    if (stored === null) return createDefaultBrowserPreferences()
    if (utf8Length(stored) > MAX_BROWSER_PREFERENCES_BYTES) {
      return createDefaultBrowserPreferences()
    }
    return decodeBrowserPreferences(JSON.parse(stored))
  } catch {
    return createDefaultBrowserPreferences()
  }
}

export const saveBrowserPreferences = (preferences: BrowserPreferences): void => {
  try {
    if (typeof localStorage === 'undefined') return
    localStorage.setItem(BROWSER_PREFERENCES_KEY, JSON.stringify(preferences))
  } catch {
    // Preferences remain available in Redux when browser storage is unavailable.
  }
}
