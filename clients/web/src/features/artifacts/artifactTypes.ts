import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'

// Tunable effective ceilings: these keep initial rendering and later operator-requested work
// predictable. They are presentation budgets, not security boundaries; the owning transport must
// independently bound bytes received and decoded before content reaches these renderers.
export const ARTIFACT_PREVIEW_CHARACTERS = 4_000
export const ARTIFACT_PREVIEW_LINES = 32
// The expanded projection is deliberately larger but still finite so one action cannot mount or
// highlight an entire large artifact. These values can be tuned from measured interaction costs.
export const ARTIFACT_EXPANDED_CHARACTERS = 16_000
export const ARTIFACT_EXPANDED_LINES = 200

interface ArtifactIdentity {
  id: string
  displayName: string
}

export interface TextArtifact extends ArtifactIdentity {
  kind: 'text'
  // The owning input boundary supplies at most the expanded projection and the full count.
  content: string
  characterCount: number
}

export interface CodeArtifact extends ArtifactIdentity {
  kind: 'code'
  // The owning input boundary supplies at most the expanded projection and the full count.
  content: string
  characterCount: number
  language: string
}

export interface SignalboxImageArtifact extends ArtifactIdentity {
  kind: 'image'
  source: { kind: 'signalbox_blob'; descriptor: WebBlobDescriptor }
}

export interface RemoteImageArtifact extends ArtifactIdentity {
  kind: 'image'
  source: { kind: 'remote'; url: string; alt: string }
}

export interface DocumentArtifact extends ArtifactIdentity {
  kind: 'document'
  source: { kind: 'signalbox_blob'; descriptor: WebBlobDescriptor }
  documentKind: 'pdf' | 'document'
}

export interface DerivativeArtifact extends ArtifactIdentity {
  kind: 'derivative'
  source: { kind: 'signalbox_blob'; descriptor: WebBlobDescriptor }
  viewKind: 'preview' | 'thumbnail'
  presentation: 'image'
}

export interface MediaPlaceholderArtifact extends ArtifactIdentity {
  kind: 'media_placeholder'
  mediaKind: 'audio' | 'video'
  source: { kind: 'signalbox_blob'; descriptor: WebBlobDescriptor }
}

export interface GenericBlobArtifact extends ArtifactIdentity {
  kind: 'blob'
  descriptor: WebBlobDescriptor
}

export interface BlockedArtifact extends ArtifactIdentity {
  kind: 'blocked'
  attemptedKind: string
  reason: string
}

export interface CommittedUnimplementedArtifact extends ArtifactIdentity {
  kind: 'committed_unimplemented'
  attemptedKind: string
}

export type RenderableArtifact =
  | TextArtifact
  | CodeArtifact
  | SignalboxImageArtifact
  | RemoteImageArtifact
  | DocumentArtifact
  | DerivativeArtifact
  | MediaPlaceholderArtifact
  | GenericBlobArtifact

export type ArtifactItem = RenderableArtifact | BlockedArtifact | CommittedUnimplementedArtifact

export interface BoundedArtifactText {
  content: string
  omittedCharacters: number
  omittedLines: boolean
}

export type ArtifactTextMode = 'preview' | 'expanded'

export const boundArtifactText = (
  content: string,
  totalCharacters: number,
  mode: ArtifactTextMode,
): BoundedArtifactText => {
  const expanded = mode === 'expanded'
  const characterLimit = expanded ? ARTIFACT_EXPANDED_CHARACTERS : ARTIFACT_PREVIEW_CHARACTERS
  const lineLimit = expanded ? ARTIFACT_EXPANDED_LINES : ARTIFACT_PREVIEW_LINES
  let characterPrefix = ''
  let prefixCharacters = 0
  for (const character of content) {
    if (prefixCharacters === characterLimit) break
    characterPrefix += character
    prefixCharacters += 1
  }
  const lines = characterPrefix.split('\n', lineLimit + 1)
  const omittedLines = lines.length > lineLimit
  const boundedLines = omittedLines ? lines.slice(0, lineLimit) : lines
  const bounded = boundedLines.join('\n')
  let boundedCharacterCount = 0
  for (const _character of bounded) boundedCharacterCount += 1

  return {
    content: bounded,
    omittedCharacters: Math.max(totalCharacters - boundedCharacterCount, 0),
    omittedLines,
  }
}
