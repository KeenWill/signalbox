import type { DensityMode, DetailMode, LayoutMode, ThemeMode } from './state'

export interface BrowserPreferences {
  layout: LayoutMode
  density: DensityMode
  detail: DetailMode
  theme: ThemeMode
}

export const defaultBrowserPreferences: BrowserPreferences = {
  layout: 'workbench',
  density: 'compact',
  detail: 'condensed',
  theme: 'dark',
}

export const BROWSER_PREFERENCES_KEY = 'signalbox.web.preferences.v1'
const hasExactKeys = (candidate: Record<string, unknown>, keys: readonly string[]) =>
  Object.keys(candidate).length === keys.length && keys.every((key) => key in candidate)

export const decodeBrowserPreferences = (value: unknown): BrowserPreferences => {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    return defaultBrowserPreferences
  }
  const candidate = value as Record<string, unknown>
  if (
    !hasExactKeys(candidate, ['layout', 'density', 'detail', 'theme']) ||
    (candidate.layout !== 'focus' && candidate.layout !== 'workbench') ||
    (candidate.density !== 'compact' && candidate.density !== 'comfortable') ||
    (candidate.detail !== 'full' &&
      candidate.detail !== 'condensed' &&
      candidate.detail !== 'results') ||
    (candidate.theme !== 'light' && candidate.theme !== 'dark')
  ) {
    return defaultBrowserPreferences
  }
  return {
    layout: candidate.layout,
    density: candidate.density,
    detail: candidate.detail,
    theme: candidate.theme,
  }
}

export const loadBrowserPreferences = (): BrowserPreferences => {
  if (typeof localStorage === 'undefined') return defaultBrowserPreferences
  try {
    const stored = localStorage.getItem(BROWSER_PREFERENCES_KEY)
    if (stored === null) return defaultBrowserPreferences
    return decodeBrowserPreferences(JSON.parse(stored))
  } catch {
    return defaultBrowserPreferences
  }
}

export const saveBrowserPreferences = (preferences: BrowserPreferences): void => {
  if (typeof localStorage === 'undefined') return
  try {
    localStorage.setItem(BROWSER_PREFERENCES_KEY, JSON.stringify(preferences))
  } catch {
    // Browser policy may make storage unavailable after startup.
  }
}
