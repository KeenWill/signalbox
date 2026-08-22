import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'

export const ARTIFACT_PREVIEW_CHARACTERS = 4_000
export const ARTIFACT_EXPANDED_CHARACTERS = 16_000
export const ARTIFACT_PREVIEW_LINES = 32
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
  const characterPrefix = content.slice(0, characterLimit)
  const lines = characterPrefix.split('\n', lineLimit + 1)
  const omittedLines = lines.length > lineLimit
  const boundedLines = omittedLines ? lines.slice(0, lineLimit) : lines
  const bounded = boundedLines.join('\n')

  return {
    content: bounded,
    omittedCharacters: Math.max(content.length - bounded.length, 0),
    omittedLines,
  }
}
