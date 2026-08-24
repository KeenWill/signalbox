import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  BootstrapContractError,
  MAX_BOOTSTRAP_RESPONSE_BYTES,
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
