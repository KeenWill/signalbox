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
  content: string
}

export interface CodeArtifact extends ArtifactIdentity {
  kind: 'code'
  content: string
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

export type ArtifactItem = RenderableArtifact | BlockedArtifact | CommittedUnimplementedArtifact

export interface BoundedArtifactText {
  content: string
  omittedCharacters: number
  omittedLines: boolean
}

export const boundArtifactText = (content: string, expanded: boolean): BoundedArtifactText => {
  const characterLimit = expanded ? ARTIFACT_EXPANDED_CHARACTERS : ARTIFACT_PREVIEW_CHARACTERS
  const lineLimit = expanded ? ARTIFACT_EXPANDED_LINES : ARTIFACT_PREVIEW_LINES
  const characters = Array.from(content)
  const characterPrefix = characters.slice(0, characterLimit).join('')
  const lines = characterPrefix.split('\n', lineLimit + 1)
  const omittedLines = lines.length > lineLimit
  const boundedLines = omittedLines ? lines.slice(0, lineLimit) : lines
  const bounded = boundedLines.join('\n')
  const boundedCharacterCount = Array.from(bounded).length

  return {
    content: bounded,
    omittedCharacters: Math.max(characters.length - boundedCharacterCount, 0),
    omittedLines,
  }
}
