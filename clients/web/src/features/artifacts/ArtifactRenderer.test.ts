import { describe, expect, it } from 'vitest'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { selectImageView } from './ArtifactRenderer'
import { imageArtifact } from './artifactScenario'

describe('artifact renderer compatibility', () => {
  it('selects the admitted view kind without interpreting its MIME string', () => {
    const descriptor: WebBlobDescriptor = {
      ...imageArtifact,
      available_views: imageArtifact.available_views.map((view) =>
        view.kind === 'preview' ? { ...view, media_type: 'application/octet-stream' } : view,
      ),
    }

    expect(selectImageView(descriptor)?.kind).toBe('preview')
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
