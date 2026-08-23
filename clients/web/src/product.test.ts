import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  MAX_BOOTSTRAP_RESPONSE_BYTES,
  productRoutes,
  productSurfaceCacheLabel,
  productSurfaceStates,
  SameOriginProductTransport,
} from './product'
import { hasValidSessionTimelineContract } from './session-timeline/model'

const bootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '1' },
  capabilities: {
    bounded_json: true,
    bounded_session_timeline: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
  },
  limits: {
    max_json_body_bytes: 65_536,
    max_ndjson_item_bytes: 262_144,
    max_timeline_window_items: 256,
    max_timeline_window_bytes: 65_536,
  },
} as const

afterEach(() => vi.unstubAllGlobals())

describe('SameOriginProductTransport', () => {
  it('decodes the Rust-authored bootstrap contract', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(bootstrapFixture))),
    )

    const bootstrap = await new SameOriginProductTransport().readBootstrap()

    expect(bootstrap).toEqual(bootstrapFixture)
  })

  it('fails closed when the daemon returns an unknown contract shape', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify({ invented: true }))),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      'bootstrap.contract',
    )
  })

  it('reports an unsuccessful HTTP response without decoding its body', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(null, { status: 503 })),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow('status 503')
  })

  it('rejects an oversized bootstrap before JSON decoding', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(' '.repeat(MAX_BOOTSTRAP_RESPONSE_BYTES + 1))),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      'encoded byte ceiling',
    )
  })
})

describe('product surface availability', () => {
  it('requires bounded JSON before enabling timeline reads', () => {
    expect(
      hasValidSessionTimelineContract({
        ...bootstrapFixture,
        capabilities: { ...bootstrapFixture.capabilities, bounded_json: false },
      }),
    ).toBe(false)
  })

  it('rejects timeline capability with unusable semantic limits', () => {
    expect(hasValidSessionTimelineContract(bootstrapFixture)).toBe(true)
    expect(
      hasValidSessionTimelineContract({
        ...bootstrapFixture,
        limits: { ...bootstrapFixture.limits, max_timeline_window_items: 0 },
      }),
    ).toBe(false)
    expect(
      hasValidSessionTimelineContract({
        ...bootstrapFixture,
        limits: { ...bootstrapFixture.limits, max_timeline_window_bytes: 65_537 },
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

  it('reports cache ownership only for implemented surfaces', () => {
    expect(productSurfaceCacheLabel('sessions')).toBe('Bounded query')
    expect(productSurfaceCacheLabel('settings')).toBe('Local settings')
    expect(productSurfaceCacheLabel('attention')).toBeNull()
  })
})
