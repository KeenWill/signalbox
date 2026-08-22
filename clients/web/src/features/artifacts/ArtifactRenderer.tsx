import {
  Ban,
  Braces,
  Download,
  FileCode2,
  FileQuestion,
  FileText,
  Image as ImageIcon,
  Maximize2,
  Minimize2,
  ShieldAlert,
} from 'lucide-react'
import type { ComponentType, ReactNode } from 'react'
import { type CommandContext, invokeCommand } from '../../commands'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { useAppSelector } from '../../state'
import { artifactScenario } from './artifactScenario'
import {
  ARTIFACT_PREVIEW_CHARACTERS,
  type ArtifactItem,
  boundArtifactText,
  type CodeArtifact,
  type GenericBlobArtifact,
  type RemoteImageArtifact,
  type RenderableArtifact,
  type SignalboxImageArtifact,
  type TextArtifact,
} from './artifactTypes'
import { admitRemoteMediaUrl } from './remoteMediaPreference'
import './artifacts.css'

type WebBlobAvailableView = WebBlobDescriptor['available_views'][number]
type WebBlobViewKind = WebBlobAvailableView['kind']
type SupportedArtifactKind = RenderableArtifact['kind']

const IMAGE_VIEW_PRIORITY: ReadonlyArray<WebBlobViewKind> = ['preview', 'thumbnail']

export const selectImageView = (descriptor: WebBlobDescriptor): WebBlobAvailableView | undefined =>
  IMAGE_VIEW_PRIORITY.map((kind) =>
    descriptor.available_views.find((view) => view.kind === kind),
  ).find((view) => view !== undefined)

export const imageViewLabel = (kind: WebBlobViewKind): string => {
  if (kind === 'browser_native') return 'Original'
  if (kind === 'thumbnail') return 'Thumbnail'
  return 'Preview'
}

const viewByKind = (
  descriptor: WebBlobDescriptor,
  kind: WebBlobViewKind,
): WebBlobAvailableView | undefined => descriptor.available_views.find((view) => view.kind === kind)

interface RendererProps<T extends RenderableArtifact> {
  artifact: T
  commandContext: CommandContext
}

type ArtifactCommandId =
  | 'artifact.preview.expand'
  | 'artifact.preview.collapse'
  | 'artifact.original.load'

const selectArtifact = (commandContext: CommandContext, artifactId: string) => {
  invokeCommand('artifact.select', {
    ...commandContext,
    artifactSelectionTarget: artifactId,
  })
}

const invokeArtifactAction = (
  commandContext: CommandContext,
  commandId: ArtifactCommandId,
  artifactId: string,
) => {
  selectArtifact(commandContext, artifactId)
  invokeCommand(commandId, commandContext)
}

function TextBody({ artifact, commandContext }: RendererProps<TextArtifact>) {
  const expanded = useAppSelector((state) => Boolean(state.app.expandedArtifacts[artifact.id]))
  const bounded = boundArtifactText(
    artifact.content,
    artifact.characterCount,
    expanded ? 'expanded' : 'preview',
  )
  const canExpand = !expanded && bounded.omittedCharacters > 0

  return (
    <div className="artifact-rendered artifact-text">
      <textarea
        className="artifact-scroll"
        aria-label={`Bounded preview of ${artifact.displayName}`}
        readOnly
        value={bounded.content}
      />
      <BoundedFooter
        omittedCharacters={bounded.omittedCharacters}
        canExpand={canExpand}
        expanded={expanded}
        onToggle={() =>
          invokeArtifactAction(
            commandContext,
            expanded ? 'artifact.preview.collapse' : 'artifact.preview.expand',
            artifact.id,
          )
        }
      />
    </div>
  )
}

function CodeBody({ artifact, commandContext }: RendererProps<CodeArtifact>) {
  const expanded = useAppSelector((state) => Boolean(state.app.expandedArtifacts[artifact.id]))
  const bounded = boundArtifactText(
    artifact.content,
    artifact.characterCount,
    expanded ? 'expanded' : 'preview',
  )
  const canExpand = !expanded && bounded.omittedCharacters > 0

  return (
    <div className="artifact-rendered artifact-code">
      <div className="artifact-code-heading">
        <Braces aria-hidden="true" />
        <span>{artifact.language}</span>
      </div>
      <textarea
        className="artifact-scroll"
        aria-label={`Bounded preview of ${artifact.displayName}`}
        readOnly
        value={bounded.content}
      />
      <BoundedFooter
        omittedCharacters={bounded.omittedCharacters}
        canExpand={canExpand}
        expanded={expanded}
        onToggle={() =>
          invokeArtifactAction(
            commandContext,
            expanded ? 'artifact.preview.collapse' : 'artifact.preview.expand',
            artifact.id,
          )
        }
      />
    </div>
  )
}

function BoundedFooter({
  omittedCharacters,
  canExpand,
  expanded,
  onToggle,
}: {
  omittedCharacters: number
  canExpand: boolean
  expanded: boolean
  onToggle: () => void
}) {
  return (
    <footer className="artifact-bounded-footer">
      <span>
        {omittedCharacters > 0
          ? `${omittedCharacters.toLocaleString()} characters remain outside this bounded view`
          : 'Complete bounded content shown'}
      </span>
      {(canExpand || expanded) && (
        <button type="button" onClick={onToggle}>
          {expanded ? <Minimize2 aria-hidden="true" /> : <Maximize2 aria-hidden="true" />}
          {expanded ? 'Collapse preview' : 'Expand bounded preview'}
        </button>
      )}
    </footer>
  )
}

function SignalboxImageBody({ artifact, commandContext }: RendererProps<SignalboxImageArtifact>) {
  const originalRequested = useAppSelector((state) =>
    Boolean(state.app.originalArtifacts[artifact.id]),
  )
  const { descriptor } = artifact.source
  const automatic = selectImageView(descriptor)
  const original = viewByKind(descriptor, 'browser_native')
  const download = viewByKind(descriptor, 'download')
  const rendered = originalRequested && original ? original : automatic
  const derivation = rendered?.derivations[0]

  return (
    <div className="artifact-image-layout">
      <div className="artifact-visual">
        {rendered ? (
          <img
            src={rendered.content_url}
            alt={`${imageViewLabel(rendered.kind)} of ${artifact.displayName}`}
            loading="lazy"
          />
        ) : (
          <FileQuestion aria-label="No compatible inline renderer" />
        )}
      </div>
      <ArtifactMetadata
        renderer={rendered?.kind ?? 'metadata fallback'}
        mediaType={descriptor.declared_media_type}
        provenance={derivation?.transformation_name ?? 'original bytes'}
      >
        {original && (
          <button
            type="button"
            aria-pressed={originalRequested}
            onClick={() =>
              invokeArtifactAction(commandContext, 'artifact.original.load', artifact.id)
            }
          >
            <Maximize2 aria-hidden="true" />
            {originalRequested ? 'Original loaded' : 'Load original'}
          </button>
        )}
        {download && (
          <a href={download.content_url} download={artifact.displayName}>
            <Download aria-hidden="true" /> Download
          </a>
        )}
      </ArtifactMetadata>
    </div>
  )
}

function RemoteImageBody({ artifact }: RendererProps<RemoteImageArtifact>) {
  const admittedUrl = admitRemoteMediaUrl(artifact.source.url)

  return (
    <div className="artifact-image-layout">
      <div className="artifact-visual remote-media">
        <Ban aria-label="Remote media not loaded" />
      </div>
      <ArtifactMetadata
        renderer={admittedUrl === null ? 'remote media blocked' : 'remote media unavailable'}
        mediaType="Not inspected"
        provenance="External URL"
      >
        {admittedUrl !== null && (
          <p>Remote rendering requires a bounded owning media service. No bytes were fetched.</p>
        )}
      </ArtifactMetadata>
    </div>
  )
}

function GenericBlobBody({ artifact }: RendererProps<GenericBlobArtifact>) {
  const download = viewByKind(artifact.descriptor, 'download')

  return (
    <div className="artifact-image-layout">
      <div className="artifact-visual">
        <FileQuestion aria-label="No compatible inline renderer" />
      </div>
      <ArtifactMetadata
        renderer="metadata fallback"
        mediaType={artifact.descriptor.declared_media_type}
        provenance="original bytes"
      >
        {download && (
          <a href={download.content_url} download={artifact.displayName}>
            <Download aria-hidden="true" /> Download
          </a>
        )}
      </ArtifactMetadata>
    </div>
  )
}

const isSignalboxImage = (
  artifact: SignalboxImageArtifact | RemoteImageArtifact,
): artifact is SignalboxImageArtifact => artifact.source.kind === 'signalbox_blob'

function ImageBody({
  artifact,
  commandContext,
}: RendererProps<SignalboxImageArtifact | RemoteImageArtifact>) {
  return isSignalboxImage(artifact) ? (
    <SignalboxImageBody artifact={artifact} commandContext={commandContext} />
  ) : (
    <RemoteImageBody artifact={artifact} commandContext={commandContext} />
  )
}

function ArtifactMetadata({
  renderer,
  mediaType,
  provenance,
  children,
}: {
  renderer: string
  mediaType: string
  provenance: string
  children: ReactNode
}) {
  return (
    <div className="artifact-detail">
      <dl>
        <div>
          <dt>Renderer</dt>
          <dd>{renderer}</dd>
        </div>
        <div>
          <dt>Declared type</dt>
          <dd>{mediaType}</dd>
        </div>
        <div>
          <dt>Provenance</dt>
          <dd>{provenance}</dd>
        </div>
      </dl>
      <div className="artifact-actions">{children}</div>
    </div>
  )
}

const rendererRegistry: {
  [Kind in SupportedArtifactKind]: ComponentType<
    RendererProps<Extract<RenderableArtifact, { kind: Kind }>>
  >
} = {
  text: TextBody,
  code: CodeBody,
  image: ImageBody,
  blob: GenericBlobBody,
}

export const registeredArtifactKinds = Object.freeze(Object.keys(rendererRegistry).sort())

function RendererBoundary({
  artifact,
  commandContext,
}: {
  artifact: ArtifactItem
  commandContext: CommandContext
}) {
  if (artifact.kind === 'blocked') {
    return (
      <div className="artifact-state blocked" role="status">
        <ShieldAlert aria-hidden="true" />
        <div>
          <strong>Artifact blocked</strong>
          <p>{artifact.reason}</p>
        </div>
      </div>
    )
  }
  const Renderer = rendererRegistry[artifact.kind] as ComponentType<RendererProps<typeof artifact>>
  return <Renderer artifact={artifact} commandContext={commandContext} />
}

const artifactIcon = (artifact: ArtifactItem) => {
  if (artifact.kind === 'text') return <FileText aria-hidden="true" />
  if (artifact.kind === 'code') return <FileCode2 aria-hidden="true" />
  if (artifact.kind === 'image') return <ImageIcon aria-hidden="true" />
  if (artifact.kind === 'blob') return <FileQuestion aria-hidden="true" />
  if (artifact.kind === 'blocked') return <ShieldAlert aria-hidden="true" />
  return <FileQuestion aria-hidden="true" />
}

export function ArtifactRenderer({
  artifact,
  commandContext,
}: {
  artifact: ArtifactItem
  commandContext: CommandContext
}) {
  const selected = useAppSelector((state) => state.app.selectedArtifact === artifact.id)
  return (
    <article
      className="artifact-row"
      aria-label={`Artifact ${artifact.displayName}`}
      data-selected={selected || undefined}
    >
      <button
        type="button"
        className="artifact-heading"
        aria-pressed={selected}
        onClick={() => selectArtifact(commandContext, artifact.id)}
      >
        {artifactIcon(artifact)}
        <div>
          <strong>{artifact.displayName}</strong>
          <small>{artifact.kind === 'blocked' ? artifact.attemptedKind : artifact.kind}</small>
        </div>
      </button>
      <RendererBoundary artifact={artifact} commandContext={commandContext} />
    </article>
  )
}

export function ArtifactWorkbench({ commandContext }: { commandContext: CommandContext }) {
  return (
    <section className="artifact-panel" aria-labelledby="artifact-heading">
      <header className="section-header artifact-panel-heading">
        <div>
          <span className="eyebrow">Typed capability projection</span>
          <h1 id="artifact-heading">Artifact renderers</h1>
        </div>
      </header>
      <p className="artifact-bound-summary">
        {artifactScenario.length} typed records · {ARTIFACT_PREVIEW_CHARACTERS.toLocaleString()}
        -character previews · 0 original bytes prefetched
      </p>
      <div className="artifact-list">
        {artifactScenario.map((artifact) => (
          <ArtifactRenderer key={artifact.id} artifact={artifact} commandContext={commandContext} />
        ))}
      </div>
    </section>
  )
}
