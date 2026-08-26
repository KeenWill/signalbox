import { describe, expect, it } from 'vitest'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { imageViewLabel, selectImageView } from './ArtifactRenderer'
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
})
