import { describe, expect, it } from 'vitest'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import {
  imageViewLabel,
  isAnimationSafeImageHeader,
  isInlineOriginalByteLengthAdmitted,
  isInlineOriginalLengthAdmitted,
  MAX_INLINE_ORIGINAL_BYTES,
  MAX_INLINE_ORIGINAL_PIXELS,
  readImageDimensions,
  selectImageView,
} from './ArtifactRenderer'
import { imageArtifact } from './artifactScenario'

const download = imageArtifact.available_views[0]
const browserNative = imageArtifact.available_views[1]
const preview = imageArtifact.available_views[2]
if (!download || !browserNative || !preview) {
  throw new Error('the image artifact fixture must contain download, original, and preview views')
}

describe('artifact renderer compatibility', () => {
  it('selects the admitted view kind without interpreting its MIME string', () => {
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
          content_url: imageArtifact.available_views[0]?.content_url ?? '',
          derivations: [],
        },
      ],
    }

    expect(selectImageView(descriptor)).toBeUndefined()
  })

  it('keeps a browser-native original behind explicit loading', () => {
    const descriptor: WebBlobDescriptor = {
      ...imageArtifact,
      available_views: imageArtifact.available_views.filter(
        (view) => view.kind === 'browser_native' || view.kind === 'download',
      ),
    }

    expect(selectImageView(descriptor)).toBeUndefined()
  })

  it('admits an original at the inline byte ceiling', () => {
    expect(isInlineOriginalByteLengthAdmitted(String(MAX_INLINE_ORIGINAL_BYTES))).toBe(true)
  })

  it('rejects an original beyond the inline byte ceiling', () => {
    expect(isInlineOriginalByteLengthAdmitted(String(MAX_INLINE_ORIGINAL_BYTES + 1))).toBe(false)
  })

  it('rejects an original whose view length differs from the immutable blob length', () => {
    expect(isInlineOriginalLengthAdmitted('16777217', '1024')).toBe(false)
  })

  it('reads bounded PNG dimensions for pixel admission', () => {
    const bytes = new Uint8Array(24)
    const view = new DataView(bytes.buffer)
    view.setUint32(0, 0x89504e47)
    view.setUint32(4, 0x0d0a1a0a)
    view.setUint32(16, 800)
    view.setUint32(20, 600)

    expect(readImageDimensions(bytes)).toEqual({ width: 800, height: 600 })
  })

  it('reads bounded GIF dimensions for pixel admission', () => {
    const bytes = new Uint8Array(10)
    bytes.set(new TextEncoder().encode('GIF89a'))
    const view = new DataView(bytes.buffer)
    view.setUint16(6, 320, true)
    view.setUint16(8, 240, true)

    expect(readImageDimensions(bytes)).toEqual({ width: 320, height: 240 })
  })

  it('reads bounded JPEG dimensions for pixel admission', () => {
    const bytes = new Uint8Array([
      0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x01, 0xe0,
      0x02, 0x80, 0x03, 0x01, 0x11, 0x00, 0xff, 0xd9,
    ])

    expect(readImageDimensions(bytes)).toEqual({ width: 640, height: 480 })
  })

  it('skips legal JPEG marker fill bytes', () => {
    const bytes = new Uint8Array([
      0xff, 0xd8, 0xff, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xff, 0xc0, 0x00, 0x0b, 0x08,
      0x01, 0xe0, 0x02, 0x80, 0x03, 0x01, 0x11, 0x00, 0xff, 0xd9,
    ])

    expect(readImageDimensions(bytes)).toEqual({ width: 640, height: 480 })
  })

  it('skips the standalone JPEG TEM marker', () => {
    const bytes = new Uint8Array([
      0xff, 0xd8, 0xff, 0x01, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x01, 0xe0, 0x02, 0x80, 0x03, 0x01,
      0x11, 0x00, 0xff, 0xd9,
    ])

    expect(readImageDimensions(bytes)).toEqual({ width: 640, height: 480 })
  })

  it('admits a valid single-frame GIF and rejects a multi-frame GIF', () => {
    const header = [0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]
    const frame = [
      0x2c, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x01, 0x00, 0x00,
    ]
    expect(isAnimationSafeImageHeader(new Uint8Array([...header, ...frame, 0x3b]))).toBe(true)
    expect(isAnimationSafeImageHeader(new Uint8Array([...header, ...frame, ...frame, 0x3b]))).toBe(
      false,
    )
  })

  it('keeps other animation-capable originals download-only', () => {
    const animatedWebp = new Uint8Array(30)
    animatedWebp.set(new TextEncoder().encode('RIFF'), 0)
    animatedWebp.set(new TextEncoder().encode('WEBP'), 8)
    animatedWebp.set(new TextEncoder().encode('VP8X'), 12)
    animatedWebp[20] = 0x02
    expect(isAnimationSafeImageHeader(animatedWebp)).toBe(false)
  })

  it('reads PNG chunk tags by byte offset after multibyte metadata', () => {
    const bytes = new Uint8Array(34)
    bytes.set([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])
    const view = new DataView(bytes.buffer)
    view.setUint32(8, 2)
    bytes.set(new TextEncoder().encode('tEXt'), 12)
    bytes.set([0xc3, 0xa9], 16)
    view.setUint32(22, 0)
    bytes.set(new TextEncoder().encode('IDAT'), 26)

    expect(isAnimationSafeImageHeader(bytes)).toBe(true)
  })

  it('reads bounded extended WebP dimensions for pixel admission', () => {
    const bytes = new Uint8Array(30)
    bytes.set(new TextEncoder().encode('RIFF'), 0)
    bytes.set(new TextEncoder().encode('WEBP'), 8)
    bytes.set(new TextEncoder().encode('VP8X'), 12)
    bytes.set([0x7f, 0x02, 0x00], 24)
    bytes.set([0xdf, 0x01, 0x00], 27)

    expect(readImageDimensions(bytes)).toEqual({ width: 640, height: 480 })
  })

  it('reads bounded lossy WebP dimensions for pixel admission', () => {
    const bytes = new Uint8Array(30)
    bytes.set(new TextEncoder().encode('RIFF'), 0)
    bytes.set(new TextEncoder().encode('WEBP'), 8)
    bytes.set(new TextEncoder().encode('VP8 '), 12)
    bytes.set([0x9d, 0x01, 0x2a], 23)
    const view = new DataView(bytes.buffer)
    view.setUint16(26, 640, true)
    view.setUint16(28, 480, true)

    expect(readImageDimensions(bytes)).toEqual({ width: 640, height: 480 })
  })

  it('reads bounded lossless WebP dimensions for pixel admission', () => {
    const bytes = new Uint8Array(25)
    bytes.set(new TextEncoder().encode('RIFF'), 0)
    bytes.set(new TextEncoder().encode('WEBP'), 8)
    bytes.set(new TextEncoder().encode('VP8L'), 12)
    bytes.set([0x2f, 0x7f, 0xc2, 0x77, 0x00], 20)

    expect(readImageDimensions(bytes)).toEqual({ width: 640, height: 480 })
  })

  it('exposes dimensions beyond the decoded-pixel ceiling', () => {
    const bytes = new Uint8Array(24)
    const view = new DataView(bytes.buffer)
    view.setUint32(0, 0x89504e47)
    view.setUint32(4, 0x0d0a1a0a)
    view.setUint32(16, MAX_INLINE_ORIGINAL_PIXELS)
    view.setUint32(20, 2)

    const dimensions = readImageDimensions(bytes)
    expect(dimensions && dimensions.width * dimensions.height).toBeGreaterThan(
      MAX_INLINE_ORIGINAL_PIXELS,
    )
  })

  it('rejects malformed or unsupported image headers', () => {
    expect(readImageDimensions(new TextEncoder().encode('not an image'))).toBeNull()
  })

  it('rejects a truncated JPEG segment', () => {
    expect(
      readImageDimensions(new Uint8Array([0xff, 0xd8, 0xff, 0xe0, 0x00, 0x20, 0x00, 0x00])),
    ).toBeNull()
  })
})
