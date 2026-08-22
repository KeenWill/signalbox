import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  decodeBrowserPreferences,
  defaultBrowserPreferences,
  loadBrowserPreferences,
  MAX_KEY_OVERRIDES,
  MAX_SAVED_LOGICAL_POSITIONS,
} from './preferences'

afterEach(() => vi.unstubAllGlobals())

describe('browser preferences', () => {
  it('fails closed to defaults for an unrelated stored value', () => {
    expect(decodeBrowserPreferences('not-an-object')).toEqual(defaultBrowserPreferences)
  })

  it('falls back to defaults when preference storage is unavailable', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new DOMException('Storage access denied', 'SecurityError')
      },
    })

    expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
  })

  it('clamps pane sizes and rejects unknown closed variants', () => {
    const stored = {
      layout: 'dashboard',
      density: 'comfortable',
      paneSizes: { navigation: -50, inspector: 50_000 },
      remoteMedia: 'proxy',
    } as const
    const decoded = decodeBrowserPreferences(stored)

    expect(decoded.layout).toBe(defaultBrowserPreferences.layout)
    expect(decoded.density).toBe(stored.density)
    expect(decoded.paneSizes).toEqual({ navigation: 160, inspector: 480 })
    expect(decoded.remoteMedia).toBe(defaultBrowserPreferences.remoteMedia)
  })

  it('bounds retained positions and future key overrides', () => {
    const lastLogicalPositions = Object.fromEntries(
      Array.from({ length: MAX_SAVED_LOGICAL_POSITIONS + 3 }, (_, index) => [
        `session-${index}`,
        `cursor-${index}`,
      ]),
    )
    const keyOverrides = Object.fromEntries(
      Array.from({ length: MAX_KEY_OVERRIDES + 2 }, (_, index) => [
        `command-${index}`,
        `key-${index}`,
      ]),
    )

    const decoded = decodeBrowserPreferences({ lastLogicalPositions, keyOverrides })

    expect(Object.keys(decoded.lastLogicalPositions)).toHaveLength(MAX_SAVED_LOGICAL_POSITIONS)
    expect(Object.keys(decoded.keyOverrides)).toHaveLength(MAX_KEY_OVERRIDES)
  })
})
