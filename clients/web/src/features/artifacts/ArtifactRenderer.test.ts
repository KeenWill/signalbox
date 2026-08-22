import { describe, expect, it } from 'vitest'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { boundAttachments, MAX_VISIBLE_ATTACHMENTS } from './ArtifactAttachments'
import {
  imageViewLabel,
  registeredArtifactKinds,
  selectBlobView,
  selectImageView,
  selectViewDerivation,
} from './ArtifactRenderer'
import {
  artifactOriginalIds,
  artifactPreviewIds,
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
import { admitRemoteMediaUrl } from './remoteMediaPreference'

const download = imageArtifact.available_views[0]
const browserNative = imageArtifact.available_views[1]
const preview = imageArtifact.available_views[2]
const authoritativeDerivation = imagePreviewView.derivations[0]
if (!download || !browserNative || !preview || !authoritativeDerivation) {
  throw new Error(
    'the image artifact fixture must contain download, original, preview, and provenance',
  )
}

describe('artifact renderer compatibility', () => {
  it('derives preview command IDs only from artifacts with omitted preview content', () => {
    expect(artifactPreviewIds).toEqual(['incident-notes', 'renderer-source'])
  })

  it('derives original-capable artifact IDs from admitted descriptor views', () => {
    expect(artifactOriginalIds).toEqual(['orbital-map'])
  })

  it('registers the closed artifact renderer set', () => {
    expect(registeredArtifactKinds).toEqual([
      'blob',
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
      available_views: [
        download,
        browserNative,
        { ...preview, media_type: 'application/octet-stream' },
      ],
    }

    expect(selectImageView(descriptor)?.kind).toBe('preview')
  })

  it('selects the derivation that binds the descriptor input to the rendered output', () => {
    const unrelated = {
      ...authoritativeDerivation,
      derivation_id: '0198f321-2300-7000-8000-000000000002',
      input_digests: [`sha256:${'9a'.repeat(32)}`],
      output_digests: [`sha256:${'9b'.repeat(32)}`],
    }
    const view = {
      ...imagePreviewView,
      derivations: [unrelated, authoritativeDerivation],
    }

    expect(selectViewDerivation(imageArtifact, view)?.derivation_id).toBe(
      authoritativeDerivation.derivation_id,
    )
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

  it('admits only credential-free HTTPS remote media', () => {
    expect(admitRemoteMediaUrl('https://media.example.test/status.png')).toBe(
      'https://media.example.test/status.png',
    )
    expect(admitRemoteMediaUrl('http://media.example.test/status.png')).toBeNull()
    expect(admitRemoteMediaUrl('https://token@media.example.test/status.png')).toBeNull()
  })
})
