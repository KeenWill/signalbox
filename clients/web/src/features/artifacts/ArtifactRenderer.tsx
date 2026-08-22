import {
  Ban,
  Braces,
  Download,
  ExternalLink,
  File,
  FileAudio,
  FileCode2,
  FileQuestion,
  FileText,
  FileVideo,
  GitBranch,
  Image as ImageIcon,
  Maximize2,
  Minimize2,
  ShieldAlert,
} from 'lucide-react'
import { type ComponentType, type ReactNode, useState } from 'react'
import { type CommandContext, invokeCommand } from '../../commands'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { actions, selectApp, useAppDispatch, useAppSelector } from '../../state'
import { artifactScenario } from './artifactScenario'
import {
  ARTIFACT_PREVIEW_CHARACTERS,
  type ArtifactItem,
  boundArtifactText,
  type CodeArtifact,
  type DerivativeArtifact,
  type DocumentArtifact,
  type MediaPlaceholderArtifact,
  type RemoteImageArtifact,
  type RenderableArtifact,
  type SignalboxImageArtifact,
  type TextArtifact,
} from './artifactTypes'
import { admitRemoteMediaUrl, type RemoteMediaPolicy } from './remoteMediaPreference'
import './artifacts.css'

type WebBlobAvailableView = WebBlobDescriptor['available_views'][number]
type WebBlobViewKind = WebBlobAvailableView['kind']
type SupportedArtifactKind = RenderableArtifact['kind']

const IMAGE_VIEW_PRIORITY: ReadonlyArray<WebBlobViewKind> = ['preview', 'thumbnail']

export const selectImageView = (descriptor: WebBlobDescriptor): WebBlobAvailableView | undefined =>
  IMAGE_VIEW_PRIORITY.map((kind) =>
    descriptor.available_views.find((view) => view.kind === kind),
  ).find((view) => view !== undefined)

export const selectBlobView = (
  descriptor: WebBlobDescriptor,
  kind: WebBlobViewKind,
): WebBlobAvailableView | undefined => descriptor.available_views.find((view) => view.kind === kind)

interface RendererProps<T extends RenderableArtifact> {
  artifact: T
  remoteMediaPolicy: RemoteMediaPolicy
  commandContext: CommandContext
}

type ArtifactCommandId =
  | 'artifact.preview.expand'
  | 'artifact.preview.collapse'
  | 'artifact.original.load'
  | 'artifact.remote-policy.ask'
  | 'artifact.remote-policy.block'
  | 'artifact.remote-policy.allow'

const invokeArtifactAction = (
  commandContext: CommandContext,
  commandId: ArtifactCommandId,
  action: () => void,
) => invokeCommand(commandId, { ...commandContext, artifactAction: action })

function TextBody({ artifact, commandContext }: RendererProps<TextArtifact>) {
  const [expanded, setExpanded] = useState(false)
  const bounded = boundArtifactText(artifact.content, expanded)
  const canExpand = !expanded && bounded.omittedCharacters > 0

  return (
    <div className="artifact-rendered artifact-text">
      <pre>{bounded.content}</pre>
      <BoundedFooter
        omittedCharacters={bounded.omittedCharacters}
        canExpand={canExpand}
        expanded={expanded}
        onToggle={() =>
          invokeArtifactAction(
            commandContext,
            expanded ? 'artifact.preview.collapse' : 'artifact.preview.expand',
            () => setExpanded((current) => !current),
          )
        }
      />
    </div>
  )
}

function CodeBody({ artifact, commandContext }: RendererProps<CodeArtifact>) {
  const [expanded, setExpanded] = useState(false)
  const bounded = boundArtifactText(artifact.content, expanded)
  const canExpand = !expanded && bounded.omittedCharacters > 0

  return (
    <div className="artifact-rendered artifact-code">
      <div className="artifact-code-heading">
        <Braces aria-hidden="true" />
        <span>{artifact.language}</span>
      </div>
      <pre>
        <code>{bounded.content}</code>
      </pre>
      <BoundedFooter
        omittedCharacters={bounded.omittedCharacters}
        canExpand={canExpand}
        expanded={expanded}
        onToggle={() =>
          invokeArtifactAction(
            commandContext,
            expanded ? 'artifact.preview.collapse' : 'artifact.preview.expand',
            () => setExpanded((current) => !current),
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
  const [originalRequested, setOriginalRequested] = useState(false)
  const { descriptor } = artifact.source
  const automatic = selectImageView(descriptor)
  const original = selectBlobView(descriptor, 'browser_native')
  const download = selectBlobView(descriptor, 'download')
  const rendered = originalRequested && original ? original : automatic
  const derivation = rendered?.derivations[0]

  return (
    <div className="artifact-image-layout">
      <div className="artifact-visual">
        {rendered ? (
          <img
            src={rendered.content_url}
            alt={`${rendered.kind === 'browser_native' ? 'Original' : 'Preview'} of ${artifact.displayName}`}
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
              invokeArtifactAction(commandContext, 'artifact.original.load', () =>
                setOriginalRequested(true),
              )
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

const isSignalboxImage = (
  artifact: SignalboxImageArtifact | RemoteImageArtifact,
): artifact is SignalboxImageArtifact => artifact.source.kind === 'signalbox_blob'

function ImageBody({
  artifact,
  remoteMediaPolicy,
  commandContext,
}: RendererProps<SignalboxImageArtifact | RemoteImageArtifact>) {
  return isSignalboxImage(artifact) ? (
    <SignalboxImageBody
      artifact={artifact}
      remoteMediaPolicy={remoteMediaPolicy}
      commandContext={commandContext}
    />
  ) : (
    <RemoteImageBody
      artifact={artifact}
      remoteMediaPolicy={remoteMediaPolicy}
      commandContext={commandContext}
    />
  )
}

function DocumentBody({ artifact }: RendererProps<DocumentArtifact>) {
  const { descriptor } = artifact.source
  const browserNative = selectBlobView(descriptor, 'browser_native')
  const download = selectBlobView(descriptor, 'download')

  return (
    <div className="artifact-placeholder-layout">
      <div className="artifact-document-placeholder">
        <File aria-hidden="true" />
        <strong>{artifact.documentKind === 'pdf' ? 'PDF document' : 'Document'}</strong>
        <p>Document bytes stay unloaded until an explicit open or download action.</p>
      </div>
      <ArtifactMetadata
        renderer="document placeholder"
        mediaType={descriptor.declared_media_type}
        provenance="Original blob"
      >
        {browserNative && (
          <a href={browserNative.content_url} target="_blank" rel="noreferrer">
            <ExternalLink aria-hidden="true" /> Open document
          </a>
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

function DerivativeBody({ artifact }: RendererProps<DerivativeArtifact>) {
  const rendered = selectBlobView(artifact.source.descriptor, artifact.viewKind)
  const derivation = rendered?.derivations[0]

  if (!rendered || !derivation) {
    return (
      <div className="artifact-state blocked" role="status">
        <ShieldAlert aria-hidden="true" />
        <div>
          <strong>Derivative unavailable</strong>
          <p>The descriptor does not authorize the requested derived view and no fallback ran.</p>
        </div>
      </div>
    )
  }

  return (
    <div className="artifact-image-layout">
      <div className="artifact-visual">
        <img
          src={rendered.content_url}
          alt={`Derived ${artifact.viewKind} of ${artifact.displayName}`}
          loading="lazy"
        />
      </div>
      <ArtifactMetadata
        renderer={`${artifact.viewKind} derivative`}
        mediaType={rendered.media_type}
        provenance={`${derivation.transformation_name} v${derivation.transformation_version}`}
      >
        <span className="artifact-provenance-count">
          {derivation.input_digests.length} input · {derivation.output_digests.length} output
        </span>
      </ArtifactMetadata>
    </div>
  )
}

function MediaPlaceholderBody({ artifact }: RendererProps<MediaPlaceholderArtifact>) {
  const { descriptor } = artifact.source
  const download = selectBlobView(descriptor, 'download')
  const MediaIcon = artifact.mediaKind === 'audio' ? FileAudio : FileVideo

  return (
    <div className="artifact-placeholder-layout">
      <div className="artifact-document-placeholder media-placeholder">
        <MediaIcon aria-hidden="true" />
        <strong>{artifact.mediaKind === 'audio' ? 'Audio' : 'Video'} playback unavailable</strong>
        <p>The current contract supplies bytes but no admitted inline media presentation.</p>
      </div>
      <ArtifactMetadata
        renderer={`${artifact.mediaKind} placeholder`}
        mediaType={descriptor.declared_media_type}
        provenance="Original blob"
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
  document: DocumentBody,
  derivative: DerivativeBody,
  media_placeholder: MediaPlaceholderBody,
}

export const registeredArtifactKinds = Object.freeze(Object.keys(rendererRegistry).sort())

function RendererBoundary({
  artifact,
  remoteMediaPolicy,
  commandContext,
}: {
  artifact: ArtifactItem
  remoteMediaPolicy: RemoteMediaPolicy
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
  if (artifact.kind === 'committed_unimplemented') {
    return (
      <div className="artifact-state unimplemented" role="status">
        <FileQuestion aria-hidden="true" />
        <div>
          <strong>Typed renderer not implemented</strong>
          <p>
            The daemon has not exposed an admitted {artifact.attemptedKind} view on this branch. No
            bytes were read.
          </p>
        </div>
      </div>
    )
  }

  const Renderer = rendererRegistry[artifact.kind] as ComponentType<RendererProps<typeof artifact>>
  return (
    <Renderer
      artifact={artifact}
      remoteMediaPolicy={remoteMediaPolicy}
      commandContext={commandContext}
    />
  )
}

const artifactIcon = (artifact: ArtifactItem) => {
  if (artifact.kind === 'text') return <FileText aria-hidden="true" />
  if (artifact.kind === 'code') return <FileCode2 aria-hidden="true" />
  if (artifact.kind === 'image') return <ImageIcon aria-hidden="true" />
  if (artifact.kind === 'document') return <File aria-hidden="true" />
  if (artifact.kind === 'derivative') return <GitBranch aria-hidden="true" />
  if (artifact.kind === 'media_placeholder')
    return artifact.mediaKind === 'audio' ? (
      <FileAudio aria-hidden="true" />
    ) : (
      <FileVideo aria-hidden="true" />
    )
  if (artifact.kind === 'blocked') return <ShieldAlert aria-hidden="true" />
  return <FileQuestion aria-hidden="true" />
}

export function ArtifactRenderer({
  artifact,
  commandContext,
  remoteMediaPolicy = 'ask',
}: {
  artifact: ArtifactItem
  commandContext: CommandContext
  remoteMediaPolicy?: RemoteMediaPolicy
}) {
  return (
    <article className="artifact-row" aria-label={`Artifact ${artifact.displayName}`}>
      <header className="artifact-heading">
        {artifactIcon(artifact)}
        <div>
          <strong>{artifact.displayName}</strong>
          <small>
            {artifact.kind === 'blocked' || artifact.kind === 'committed_unimplemented'
              ? artifact.attemptedKind
              : artifact.kind}
          </small>
        </div>
      </header>
      <RendererBoundary
        artifact={artifact}
        remoteMediaPolicy={remoteMediaPolicy}
        commandContext={commandContext}
      />
    </article>
  )
}

export function ArtifactWorkbench({ commandContext }: { commandContext: CommandContext }) {
  const remoteMedia = useAppSelector(selectApp).remoteMedia
  const dispatch = useAppDispatch()

  return (
    <section className="artifact-panel" aria-labelledby="artifact-heading">
      <header className="section-header artifact-panel-heading">
        <div>
          <span className="eyebrow">Typed capability projection</span>
          <h1 id="artifact-heading">Artifact renderers</h1>
        </div>
        <label>
          Remote media
          <select
            value={remoteMedia}
            onChange={(event) => {
              const policy = event.target.value as typeof remoteMedia
              invokeArtifactAction(commandContext, `artifact.remote-policy.${policy}`, () =>
                dispatch(actions.remoteMediaSet(policy)),
              )
            }}
          >
            <option value="ask">Ask</option>
            <option value="block">Block</option>
            <option value="allow">Allow</option>
          </select>
        </label>
      </header>
      <p className="artifact-bound-summary">
        {artifactScenario.length} typed records · {ARTIFACT_PREVIEW_CHARACTERS.toLocaleString()}
        -character previews · 0 original bytes prefetched
      </p>
      <div className="artifact-list">
        {artifactScenario.map((artifact) => (
          <ArtifactRenderer
            key={artifact.id}
            artifact={artifact}
            remoteMediaPolicy={remoteMedia}
            commandContext={commandContext}
          />
        ))}
      </div>
    </section>
  )
}
