import { afterEach, describe, expect, it, vi } from 'vitest'
import { imageArtifact } from './features/artifacts/artifactScenario'
import {
  MAX_PRODUCT_JSON_BYTES,
  ProductTransportError,
  SameOriginProductTransport,
} from './product'

const bootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '2' },
  capabilities: {
    blob_derivations: true,
    bounded_json: true,
    image_derivatives: true,
    immutable_blob_content: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
  },
  limits: { max_json_body_bytes: 65_536, max_ndjson_item_bytes: 262_144 },
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

  it('rejects a bootstrap response beyond the JSON byte ceiling', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response('x'.repeat(MAX_PRODUCT_JSON_BYTES + 1), {
            headers: { 'content-type': 'application/json' },
          }),
      ),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow(
      'response exceeded the product JSON byte limit',
    )
  })

  it('classifies a rejected fetch as a transport failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => Promise.reject(new TypeError('network failed'))),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toBeInstanceOf(
      ProductTransportError,
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

  it('bounds descriptor error payloads before decoding them', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response('x'.repeat(MAX_PRODUCT_JSON_BYTES + 1), {
            status: 500,
            headers: { 'content-type': 'application/json' },
          }),
      ),
    )

    const request = new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
    })
    await expect(request).rejects.toThrow('response exceeded the product JSON byte limit')
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
