import { describe, expect, it } from 'vitest'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { boundAttachments, MAX_VISIBLE_ATTACHMENTS } from './ArtifactAttachments'
import { registeredArtifactKinds, selectBlobView, selectImageView } from './ArtifactRenderer'
import {
  documentAttachment,
  imageArtifact,
  imageDownloadView,
  imageOriginalView,
  imagePreviewView,
} from './artifactScenario'
import {
  ARTIFACT_EXPANDED_CHARACTERS,
  ARTIFACT_PREVIEW_CHARACTERS,
  boundArtifactText,
} from './artifactTypes'
import { admitRemoteMediaUrl, decodeRemoteMediaPolicy } from './remoteMediaPreference'

describe('artifact renderer compatibility', () => {
  it('registers the closed artifact renderer set', () => {
    expect(registeredArtifactKinds).toEqual([
      'code',
      'derivative',
      'document',
      'image',
      'media_placeholder',
      'text',
    ])
  })

  it('selects the admitted view kind without interpreting its MIME string', () => {
    expect(imagePreviewView.kind).toBe('preview')
    const descriptor: WebBlobDescriptor = {
      ...imageArtifact,
      available_views: [{ ...imagePreviewView, media_type: 'application/octet-stream' }],
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

  it('selects document affordances only by their admitted capability', () => {
    expect(selectBlobView(documentAttachment.source.descriptor, 'browser_native')).toBeUndefined()
    expect(selectBlobView(documentAttachment.source.descriptor, 'download')?.kind).toBe('download')
  })

  it('keeps attachment projection within its hard item ceiling', () => {
    const source = Array.from({ length: 16 }, (_, index) => ({
      ...documentAttachment,
      id: `attachment-${index}`,
    }))

    const bounded = boundAttachments(source)

    expect(bounded.visible).toHaveLength(MAX_VISIBLE_ATTACHMENTS)
    expect(bounded.omitted).toBe(source.length - MAX_VISIBLE_ATTACHMENTS)
  })

  it('bounds the initial text projection by characters', () => {
    const content = 'x'.repeat(20_000)

    const bounded = boundArtifactText(content, false)

    expect(bounded.content).toHaveLength(ARTIFACT_PREVIEW_CHARACTERS)
    expect(bounded.omittedCharacters).toBe(content.length - ARTIFACT_PREVIEW_CHARACTERS)
  })

  it('keeps expansion below the larger hard character ceiling', () => {
    const content = 'x'.repeat(20_000)

    const bounded = boundArtifactText(content, true)

    expect(bounded.content).toHaveLength(ARTIFACT_EXPANDED_CHARACTERS)
    expect(bounded.omittedCharacters).toBe(content.length - ARTIFACT_EXPANDED_CHARACTERS)
  })

  it('fails an unknown remote-media preference closed to ask', () => {
    expect(decodeRemoteMediaPolicy('invented')).toBe('ask')
  })

  it('admits only credential-free HTTPS remote media', () => {
    expect(admitRemoteMediaUrl('https://media.example.test/status.png')).toBe(
      'https://media.example.test/status.png',
    )
    expect(admitRemoteMediaUrl('http://media.example.test/status.png')).toBeNull()
    expect(admitRemoteMediaUrl('https://token@media.example.test/status.png')).toBeNull()
  })
})
