import { describe, expect, it } from 'vitest'
import { inspectedArtifact, nextResolutionSequence } from './ArtifactInspector'
import {
  fallbackDescriptor,
  imageArtifact,
  jpegDescriptor,
} from './features/artifacts/artifactScenario'

describe('artifact inspector resolution identity', () => {
  it('keeps an image declaration without admitted image views in the blob renderer', () => {
    const descriptor = { ...fallbackDescriptor, declared_media_type: 'image/svg+xml' }
    expect(inspectedArtifact(descriptor, nextResolutionSequence()).kind).toBe('blob')
  })

  it('renders admitted image views even when the declaration is not an image', () => {
    const descriptor = { ...imageArtifact, declared_media_type: 'application/octet-stream' }
    expect(inspectedArtifact(descriptor, nextResolutionSequence()).kind).toBe('image')
  })
  it('never reissues a sequence, so a remount cannot restart the count', () => {
    const first = nextResolutionSequence()
    // A route detour unmounts the inspector and discards its component state; the allocator does
    // not live there, so the next resolution continues past the identities already recorded.
    const afterRemount = nextResolutionSequence()
    expect(afterRemount).toBeGreaterThan(first)
  })

  it('gives the same digest a fresh identity for every resolution', () => {
    const earlier = inspectedArtifact(jpegDescriptor, nextResolutionSequence())
    const later = inspectedArtifact(jpegDescriptor, nextResolutionSequence())
    expect(later.id).not.toEqual(earlier.id)
  })

  it('carries the digest and resolution sequence in the identity', () => {
    const sequence = nextResolutionSequence()
    const artifact = inspectedArtifact(imageArtifact, sequence)
    expect(artifact.id).toBe(`product-artifact:${String(sequence)}:${imageArtifact.digest}`)
  })
})
