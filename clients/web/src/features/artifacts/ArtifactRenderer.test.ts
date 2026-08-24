import { describe, expect, it } from 'vitest'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { imageViewLabel, registeredArtifactKinds, selectImageView } from './ArtifactRenderer'
import {
  artifactOriginalIds,
  artifactPreviewIds,
  fetchVerifiedSingleFrameJpeg,
  INLINE_ORIGINAL_MAX_BYTES,
  imageArtifact,
  imageDownloadView,
  imageOriginalView,
  imagePreviewView,
  isSingleFrameJpegBytes,
  jpegDescriptor,
  jpegOriginalView,
  selectBoundedOriginalView,
} from './artifactScenario'
import {
  ARTIFACT_EXPANDED_CHARACTERS,
  ARTIFACT_PREVIEW_CHARACTERS,
  boundArtifactText,
} from './artifactTypes'
import { admitRemoteMediaUrl } from './remoteMediaPreference'

const download = imageArtifact.available_views[0]
const browserNative = imageArtifact.available_views[1]
const preview = imageArtifact.available_views[2]
const previewDerivation = preview?.derivations[0]
if (!download || !browserNative || !preview || !previewDerivation) {
  throw new Error(
    'the image artifact fixture must contain download, original, and preview provenance',
  )
}

describe('artifact renderer compatibility', () => {
  it('derives preview command IDs only from artifacts with omitted preview content', () => {
    expect(artifactPreviewIds).toEqual(['incident-notes', 'renderer-source'])
  })

  it('derives original-capable IDs only for reachable bounded single-frame artifacts', () => {
    expect(artifactOriginalIds).toEqual(['bounded-photo'])
    expect(selectBoundedOriginalView(imageArtifact)).toBeUndefined()
    expect(selectBoundedOriginalView(jpegDescriptor)?.kind).toBe('browser_native')
  })

  it('admits a byte-bounded, decode-proven, inherently single-frame JPEG original', () => {
    const jpegOriginal = { ...imageOriginalView, media_type: 'image/jpeg' }
    const descriptor: WebBlobDescriptor = {
      ...imageArtifact,
      declared_media_type: 'image/jpeg',
      available_views: [imageDownloadView, jpegOriginal, imagePreviewView],
    }

    expect(selectBoundedOriginalView(descriptor)).toBe(jpegOriginal)
  })

  it('keeps oversized originals download-only', () => {
    const oversizedLength = (INLINE_ORIGINAL_MAX_BYTES + 1n).toString()
    const oversizedOriginal = {
      ...imageOriginalView,
      media_type: 'image/jpeg',
      byte_length: oversizedLength,
    }
    const descriptor: WebBlobDescriptor = {
      ...imageArtifact,
      declared_media_type: 'image/jpeg',
      byte_length: oversizedLength,
      available_views: [imageDownloadView, oversizedOriginal, imagePreviewView],
    }

    expect(selectBoundedOriginalView(descriptor)).toBeUndefined()
  })

  it('keeps originals without bounded decode provenance download-only', () => {
    const jpegOriginal = { ...imageOriginalView, media_type: 'image/jpeg' }
    const descriptor: WebBlobDescriptor = {
      ...imageArtifact,
      declared_media_type: 'image/jpeg',
      available_views: [imageDownloadView, jpegOriginal],
    }

    expect(selectBoundedOriginalView(descriptor)).toBeUndefined()
  })

  it('requires one exact bounded derivation to bind both input and output', () => {
    const jpegOriginal = { ...imageOriginalView, media_type: 'image/jpeg' }
    const unrelatedDigest = `sha256:${'9a'.repeat(32)}`
    const misleadingPreview = {
      ...imagePreviewView,
      derivations: [
        { ...previewDerivation, input_digests: [unrelatedDigest] },
        {
          ...previewDerivation,
          transformation_name: 'image.thumbnail',
          input_digests: [imageArtifact.digest],
        },
      ],
    }
    const descriptor: WebBlobDescriptor = {
      ...imageArtifact,
      declared_media_type: 'image/jpeg',
      available_views: [imageDownloadView, jpegOriginal, misleadingPreview],
    }

    expect(selectBoundedOriginalView(descriptor)).toBeUndefined()
  })

  it.each(['image/gif', 'image/png', 'image/webp'])(
    'keeps animation-capable %s originals download-only without aggregate decode evidence',
    (mediaType) => {
      const descriptor: WebBlobDescriptor = {
        ...imageArtifact,
        declared_media_type: mediaType,
        available_views: [
          imageDownloadView,
          { ...imageOriginalView, media_type: mediaType },
          imagePreviewView,
        ],
      }

      expect(selectBoundedOriginalView(descriptor)).toBeUndefined()
    },
  )

  it('registers the closed text, code, and image renderer set', () => {
    expect(registeredArtifactKinds).toEqual(['blob', 'code', 'image', 'text'])
  })

  it('selects the admitted view kind without interpreting its MIME string', () => {
    expect(imagePreviewView.kind).toBe('preview')
    const descriptor: WebBlobDescriptor = {
      ...imageArtifact,
      available_views: [
        download,
        browserNative,
        { ...preview, media_type: 'application/octet-stream' },
      ],
    }

    expect(selectImageView(descriptor)?.kind).toBe('preview')
  })

  it('names a thumbnail fallback as a thumbnail', () => {
    const thumbnail = { ...preview, kind: 'thumbnail' as const }
    const descriptor: WebBlobDescriptor = {
      ...imageArtifact,
      available_views: [download, thumbnail],
    }

    expect(selectImageView(descriptor)?.kind).toBe('thumbnail')
    expect(imageViewLabel('thumbnail')).toBe('Thumbnail')
  })

  it('does not create an inline renderer from an image-like MIME string', () => {
    const descriptor: WebBlobDescriptor = {
      ...imageArtifact,
      available_views: [
        {
          kind: 'download',
          media_type: 'image/png',
          byte_length: imageArtifact.byte_length,
          content_url: imageDownloadView.content_url,
          derivations: [],
        },
      ],
    }

    expect(selectImageView(descriptor)).toBeUndefined()
  })

  it('keeps a browser-native original behind explicit loading', () => {
    expect(imageOriginalView.kind).toBe('browser_native')
    expect(imageDownloadView.kind).toBe('download')
    const descriptor: WebBlobDescriptor = {
      ...imageArtifact,
      available_views: [imageOriginalView, imageDownloadView],
    }

    expect(selectImageView(descriptor)).toBeUndefined()
  })

  it('bounds the initial text projection by characters', () => {
    const content = 'x'.repeat(20_000)

    const bounded = boundArtifactText(content, Array.from(content).length, 'preview')

    expect(bounded.content).toHaveLength(ARTIFACT_PREVIEW_CHARACTERS)
    expect(bounded.omittedCharacters).toBe(content.length - ARTIFACT_PREVIEW_CHARACTERS)
  })

  it('keeps expansion within the larger effective character ceiling', () => {
    const content = 'x'.repeat(20_000)

    const bounded = boundArtifactText(content, Array.from(content).length, 'expanded')

    expect(bounded.content).toHaveLength(ARTIFACT_EXPANDED_CHARACTERS)
    expect(bounded.omittedCharacters).toBe(content.length - ARTIFACT_EXPANDED_CHARACTERS)
  })

  it('counts and truncates Unicode by code point without splitting surrogate pairs', () => {
    const content = `${'😀'.repeat(ARTIFACT_PREVIEW_CHARACTERS)}z`

    const bounded = boundArtifactText(content, Array.from(content).length, 'preview')

    expect(Array.from(bounded.content)).toHaveLength(ARTIFACT_PREVIEW_CHARACTERS)
    expect(bounded.content.endsWith('😀')).toBe(true)
    expect(bounded.omittedCharacters).toBe(1)
  })

  it('bounds the expanded projection by lines and reports the remainder', () => {
    const content = Array.from({ length: 220 }, (_, index) => `line ${index + 1}`).join('\n')

    const bounded = boundArtifactText(content, Array.from(content).length, 'expanded')

    expect(bounded.content.split('\n')).toHaveLength(200)
    expect(bounded.omittedLines).toBe(true)
    expect(bounded.omittedCharacters).toBeGreaterThan(0)
  })

  it('counts bare carriage returns toward the expanded line ceiling', () => {
    const content = Array.from({ length: 220 }, (_, index) => `line ${index + 1}`).join('\r')

    const bounded = boundArtifactText(content, Array.from(content).length, 'expanded')

    expect(bounded.content.split('\n')).toHaveLength(200)
    expect(bounded.omittedLines).toBe(true)
    expect(bounded.omittedCharacters).toBeGreaterThan(0)
  })

  it('labels thumbnail capabilities as thumbnails', () => {
    expect(imageViewLabel('thumbnail')).toBe('Thumbnail')
  })

  it('recognizes only the JPEG start-of-image signature as single-frame evidence', () => {
    expect(isSingleFrameJpegBytes(new Uint8Array([0xff, 0xd8, 0xff, 0xe0]))).toBe(true)
    // GIF89a — animation-capable even when declared image/jpeg.
    expect(isSingleFrameJpegBytes(new Uint8Array([0x47, 0x49, 0x46, 0x38, 0x39, 0x61]))).toBe(false)
    // PNG signature — APNG shares it, so it proves nothing about frame count.
    expect(isSingleFrameJpegBytes(new Uint8Array([0x89, 0x50, 0x4e, 0x47]))).toBe(false)
    // RIFF....WEBP — animation-capable container.
    expect(isSingleFrameJpegBytes(new Uint8Array([0x52, 0x49, 0x46, 0x46]))).toBe(false)
    expect(isSingleFrameJpegBytes(new Uint8Array([0xff, 0xd8]))).toBe(false)
    expect(isSingleFrameJpegBytes(new Uint8Array([]))).toBe(false)
  })

  it('admits fetched original bytes with the JPEG signature and advertised length', async () => {
    const jpegBytes = new Uint8Array([0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10])
    const view = { ...jpegOriginalView, byte_length: String(jpegBytes.byteLength) }
    const fetchStub = (async () => new Response(jpegBytes, { status: 200 })) as typeof fetch

    const blob = await fetchVerifiedSingleFrameJpeg(view, fetchStub)

    expect(blob.size).toBe(jpegBytes.byteLength)
  })

  it('rejects fetched original bytes whose actual format is animation-capable', async () => {
    const gifBytes = new Uint8Array([0x47, 0x49, 0x46, 0x38, 0x39, 0x61])
    const view = { ...jpegOriginalView, byte_length: String(gifBytes.byteLength) }
    const fetchStub = (async () => new Response(gifBytes, { status: 200 })) as typeof fetch

    await expect(fetchVerifiedSingleFrameJpeg(view, fetchStub)).rejects.toThrow(
      'original bytes are not a single-frame JPEG stream',
    )
  })

  it('rejects fetched original bytes that diverge from the advertised length', async () => {
    const jpegBytes = new Uint8Array([0xff, 0xd8, 0xff, 0xe0])
    const view = { ...jpegOriginalView, byte_length: String(jpegBytes.byteLength + 1) }
    const fetchStub = (async () => new Response(jpegBytes, { status: 200 })) as typeof fetch

    await expect(fetchVerifiedSingleFrameJpeg(view, fetchStub)).rejects.toThrow(
      'original bytes do not match the advertised byte length',
    )
  })

  it('aborts original bytes that stream past the advertised length', async () => {
    const oversized = new Uint8Array([0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10])
    const view = { ...jpegOriginalView, byte_length: String(oversized.byteLength - 1) }
    const fetchStub = (async () => new Response(oversized, { status: 200 })) as typeof fetch

    await expect(fetchVerifiedSingleFrameJpeg(view, fetchStub)).rejects.toThrow(
      'original bytes exceed the advertised byte length',
    )
  })

  it('refuses to fetch an original advertised above the inline admission ceiling', async () => {
    const view = { ...jpegOriginalView, byte_length: (INLINE_ORIGINAL_MAX_BYTES + 1n).toString() }
    const fetchStub = (async () => {
      throw new Error('the ceiling check must reject before any request is made')
    }) as typeof fetch

    await expect(fetchVerifiedSingleFrameJpeg(view, fetchStub)).rejects.toThrow(
      'original exceeds the inline admission ceiling',
    )
  })

  it('rejects failed original responses before reading any bytes', async () => {
    const fetchStub = (async () => new Response('unavailable', { status: 500 })) as typeof fetch

    await expect(fetchVerifiedSingleFrameJpeg(jpegOriginalView, fetchStub)).rejects.toThrow(
      'original request failed with status 500',
    )
  })

  it('admits only credential-free HTTPS remote media', () => {
    expect(admitRemoteMediaUrl('https://media.example.test/status.png')).toBe(
      'https://media.example.test/status.png',
    )
    expect(admitRemoteMediaUrl('http://media.example.test/status.png')).toBeNull()
    expect(admitRemoteMediaUrl('https://token@media.example.test/status.png')).toBeNull()
  })
})
