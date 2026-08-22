import { afterEach, describe, expect, it, vi } from 'vitest'
import { binaryArtifact, imageArtifact } from './features/artifacts/artifactScenario'
import {
  MAX_DECLARED_MEDIA_TYPE_BYTES,
  MAX_DISPLAY_FILENAME_BYTES,
  MAX_PRODUCT_JSON_BYTES,
  ProductContractError,
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

const interruptedResponse = (): Response =>
  new Response(
    new ReadableStream({
      start(controller) {
        controller.error(new TypeError('response stream interrupted'))
      },
    }),
  )

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

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toBeInstanceOf(
      ProductContractError,
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

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toBeInstanceOf(
      ProductContractError,
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

  it('classifies an interrupted bootstrap response stream as a transport failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => interruptedResponse()),
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

  it('rejects a descriptor for a different immutable identity', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(binaryArtifact))),
    )

    const request = new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
    })

    await expect(request).rejects.toThrow('descriptor digest did not match')
  })

  it('rejects descriptor metadata for a different semantic use', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...imageArtifact,
              declared_media_type: 'image/jpeg',
              display_filename: ['different.jpg'],
            }),
          ),
      ),
    )

    const request = new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
      displayFilename: imageArtifact.display_filename[0],
    })

    await expect(request).rejects.toThrow('descriptor media type did not match')
  })

  it('rejects a mismatched descriptor filename', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify({ ...imageArtifact, display_filename: ['different.png'] })),
      ),
    )

    const request = new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
      displayFilename: imageArtifact.display_filename[0],
    })

    await expect(request).rejects.toThrow('descriptor filename did not match')
  })

  it('rejects an unexpected descriptor filename when none was requested', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(imageArtifact))),
    )

    const request = new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
    })

    await expect(request).rejects.toThrow('descriptor filename did not match')
  })

  it('bounds descriptor use metadata before constructing the request URL', async () => {
    const fetchRequest = vi.fn()
    vi.stubGlobal('fetch', fetchRequest)
    const transport = new SameOriginProductTransport()

    await expect(
      transport.readBlobDescriptor({
        digest: imageArtifact.digest,
        mediaType: 'x'.repeat(MAX_DECLARED_MEDIA_TYPE_BYTES + 1),
      }),
    ).rejects.toThrow('255-byte limit')
    await expect(
      transport.readBlobDescriptor({
        digest: imageArtifact.digest,
        mediaType: imageArtifact.declared_media_type,
        displayFilename: 'é'.repeat(MAX_DISPLAY_FILENAME_BYTES / 2 + 1),
      }),
    ).rejects.toThrow('1024-byte limit')
    expect(fetchRequest).not.toHaveBeenCalled()
  })

  it('accepts descriptor metadata at the exact UTF-8 ceilings', async () => {
    const mediaType = `application/${'a'.repeat(MAX_DECLARED_MEDIA_TYPE_BYTES - 'application/'.length)}`
    const displayFilename = 'é'.repeat(MAX_DISPLAY_FILENAME_BYTES / 2)
    const descriptor = {
      ...imageArtifact,
      declared_media_type: mediaType,
      display_filename: [displayFilename],
    }
    const fetchRequest = vi.fn(async () => new Response(JSON.stringify(descriptor)))
    vi.stubGlobal('fetch', fetchRequest)

    await expect(
      new SameOriginProductTransport().readBlobDescriptor({
        digest: imageArtifact.digest,
        mediaType,
        displayFilename,
      }),
    ).resolves.toEqual(descriptor)
    expect(fetchRequest).toHaveBeenCalledOnce()
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

  it('classifies an interrupted descriptor response stream as a transport failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => interruptedResponse()),
    )

    const request = new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
    })

    await expect(request).rejects.toBeInstanceOf(ProductTransportError)
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
