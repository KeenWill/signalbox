import { afterEach, describe, expect, it, vi } from 'vitest'
import { imageArtifact } from './features/artifacts/artifactScenario'
import { productRoutes, productSurfaceStates, SameOriginProductTransport } from './product'

const bootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '2' },
  capabilities: {
    blob_derivations: true,
    bounded_json: true,
    bounded_session_timeline: true,
    image_derivatives: true,
    immutable_blob_content: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
  },
  limits: {
    max_json_body_bytes: 65_536,
    max_ndjson_item_bytes: 65_536,
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

  it('resolves a blob descriptor through the generated contract decoder', async () => {
    const fetchRequest = vi.fn(async () => new Response(JSON.stringify(imageArtifact)))
    vi.stubGlobal('fetch', fetchRequest)

    const descriptor = await new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
      displayFilename: imageArtifact.display_filename[0],
    })

    expect(descriptor).toEqual(imageArtifact)
    expect(fetchRequest).toHaveBeenCalledWith(
      `/api/blobs/${encodeURIComponent(imageArtifact.digest)}/descriptor?media_type=image%2Fpng&display_filename=orbital-map.png`,
      expect.objectContaining({ credentials: 'same-origin' }),
    )
  })

  it('preserves a typed daemon error from descriptor resolution', async () => {
    const responseFixture = {
      error: {
        code: 'blob_not_found',
        kind: 'application',
        message: 'blob is not present',
      },
    } as const
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(responseFixture), { status: 404 })),
    )

    const request = new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
    })

    await expect(request).rejects.toEqual(
      expect.objectContaining({
        status: 404,
        response: responseFixture,
      }),
    )
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
      facts: ['bounded session descriptors', 'stable-address timeline windows'],
    })
  })
})
