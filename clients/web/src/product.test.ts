import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  BootstrapContractError,
  MAX_BOOTSTRAP_RESPONSE_BYTES,
  productRoutes,
  productSurfaceCacheLabel,
  productSurfaceStates,
  SameOriginProductTransport,
} from './product'
import { webContractBootstrapFixture } from './product.fixture'
import { hasValidSessionTimelineContract } from './session-timeline/model'

afterEach(() => vi.unstubAllGlobals())

describe('SameOriginProductTransport', () => {
  it('decodes the Rust-authored bootstrap contract', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(webContractBootstrapFixture))),
    )

    const bootstrap = await new SameOriginProductTransport().readBootstrap()

    expect(bootstrap).toEqual(webContractBootstrapFixture)
  })

  it('fails closed when the daemon returns an unknown contract shape', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify({ invented: true }))),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      BootstrapContractError,
    )
  })

  it('rejects malformed JSON as a contract failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('{')),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      'violates the web contract',
    )
  })

  it('rejects a bootstrap response above the fixed byte ceiling', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('x'.repeat(MAX_BOOTSTRAP_RESPONSE_BYTES + 1))),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      'exceeds the byte limit',
    )
  })

  it('reports an unsuccessful HTTP response without decoding its body', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(null, { status: 503 })),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow('status 503')
  })
})

describe('product surface availability', () => {
  it('requires bounded JSON before enabling timeline reads', () => {
    expect(
      hasValidSessionTimelineContract({
        ...webContractBootstrapFixture,
        capabilities: { ...webContractBootstrapFixture.capabilities, bounded_json: false },
      }),
    ).toBe(false)
  })

  it('rejects timeline capability with unusable semantic limits', () => {
    expect(hasValidSessionTimelineContract(webContractBootstrapFixture)).toBe(true)
    expect(
      hasValidSessionTimelineContract({
        ...webContractBootstrapFixture,
        limits: { ...webContractBootstrapFixture.limits, max_timeline_window_items: 0 },
      }),
    ).toBe(false)
    expect(
      hasValidSessionTimelineContract({
        ...webContractBootstrapFixture,
        limits: { ...webContractBootstrapFixture.limits, max_timeline_window_bytes: 65_537 },
      }),
    ).toBe(false)
  })

  it('defines one typed authority state for every product route', () => {
    expect(productSurfaceStates).toHaveProperty(productRoutes[0].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[1].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[2].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[3].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[4].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[5].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[6].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[7].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[8].id)
  })

  it('keeps Settings browser-local instead of implying daemon authority', () => {
    expect(productSurfaceStates.settings).toEqual({
      kind: 'browser-local',
      authority: 'browser preferences',
    })
  })

  it('marks the available Session timeline facts as server-backed', () => {
    expect(productSurfaceStates.sessions).toEqual({
      kind: 'server-backed',
      owningTrack: '#991 session projections',
      facts: ['bounded session descriptors', 'stable-address timeline windows'],
    })
  })

  it('marks the mounted Imports reads as server-backed', () => {
    expect(productSurfaceStates.imports).toEqual({
      kind: 'server-backed',
      owningTrack: '#995 discovery reads',
      facts: ['keyset import catalog pages', 'bounded imported-entry windows'],
    })
  })

  it('reports cache ownership only for implemented surfaces', () => {
    expect(productSurfaceCacheLabel('sessions')).toBe('Bounded query')
    expect(productSurfaceCacheLabel('imports')).toBe('Bounded query')
    expect(productSurfaceCacheLabel('settings')).toBe('Local settings')
    expect(productSurfaceCacheLabel('attention')).toBeNull()
  })
})
