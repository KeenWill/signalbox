import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  BootstrapContractError,
  MAX_BOOTSTRAP_RESPONSE_BYTES,
  productRoutes,
  productSurfaceStates,
  SameOriginProductTransport,
} from './product'
import { webContractBootstrapFixture } from './product.fixture'

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
      facts: [
        'bounded session descriptors',
        'stable-address timeline windows',
        'typed item detail pages',
      ],
    })
  })
})
