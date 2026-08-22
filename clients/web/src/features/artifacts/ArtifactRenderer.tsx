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
import { type ComponentType, type ReactNode, useState } from 'react'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { artifactScenario } from './artifactScenario'
import {
  type ArtifactItem,
  boundArtifactText,
  type CodeArtifact,
  type RemoteImageArtifact,
  type RenderableArtifact,
  type SignalboxImageArtifact,
  type TextArtifact,
} from './artifactTypes'
import {
  admitRemoteMediaUrl,
  type RemoteMediaPolicy,
  useRemoteMediaPreference,
} from './remoteMediaPreference'
import './artifacts.css'

type WebBlobAvailableView = WebBlobDescriptor['available_views'][number]
type WebBlobViewKind = WebBlobAvailableView['kind']
type SupportedArtifactKind = RenderableArtifact['kind']

const IMAGE_VIEW_PRIORITY: ReadonlyArray<WebBlobViewKind> = ['preview', 'thumbnail']

export const selectImageView = (descriptor: WebBlobDescriptor): WebBlobAvailableView | undefined =>
  IMAGE_VIEW_PRIORITY.map((kind) =>
    descriptor.available_views.find((view) => view.kind === kind),
  ).find((view) => view !== undefined)

const viewByKind = (
  descriptor: WebBlobDescriptor,
  kind: WebBlobViewKind,
): WebBlobAvailableView | undefined => descriptor.available_views.find((view) => view.kind === kind)

interface RendererProps<T extends RenderableArtifact> {
  artifact: T
  remoteMediaPolicy: RemoteMediaPolicy
}

function TextBody({ artifact }: RendererProps<TextArtifact>) {
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
        onToggle={() => setExpanded((current) => !current)}
      />
    </div>
  )
}

function CodeBody({ artifact }: RendererProps<CodeArtifact>) {
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
        onToggle={() => setExpanded((current) => !current)}
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

function SignalboxImageBody({ artifact }: RendererProps<SignalboxImageArtifact>) {
  const [originalRequested, setOriginalRequested] = useState(false)
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
        {original && !originalRequested && (
          <button type="button" onClick={() => setOriginalRequested(true)}>
            <Maximize2 aria-hidden="true" /> Load original
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

function RemoteImageBody({
  artifact,
  remoteMediaPolicy: policy,
}: RendererProps<RemoteImageArtifact>) {
  const [approved, setApproved] = useState(false)
  const admittedUrl = admitRemoteMediaUrl(artifact.source.url)
  const visible = admittedUrl !== null && (policy === 'allow' || (policy === 'ask' && approved))

  return (
    <div className="artifact-image-layout">
      <div className="artifact-visual remote-media">
        {visible ? (
          <img src={admittedUrl} alt={artifact.source.alt} loading="lazy" />
        ) : (
          <Ban aria-label="Remote media not loaded" />
        )}
      </div>
      <ArtifactMetadata
        renderer={
          admittedUrl === null
            ? 'remote media blocked'
            : visible
              ? 'remote image'
              : `remote media ${policy}`
        }
        mediaType="Not inspected"
        provenance="External URL"
      >
        {admittedUrl !== null && policy === 'ask' && !approved && (
          <button type="button" onClick={() => setApproved(true)}>
            <ImageIcon aria-hidden="true" /> Load this remote image
          </button>
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
}: RendererProps<SignalboxImageArtifact | RemoteImageArtifact>) {
  return isSignalboxImage(artifact) ? (
    <SignalboxImageBody artifact={artifact} remoteMediaPolicy={remoteMediaPolicy} />
  ) : (
    <RemoteImageBody artifact={artifact} remoteMediaPolicy={remoteMediaPolicy} />
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
}

export const registeredArtifactKinds = Object.freeze(Object.keys(rendererRegistry).sort())

function RendererBoundary({
  artifact,
  remoteMediaPolicy,
}: {
  artifact: ArtifactItem
  remoteMediaPolicy: RemoteMediaPolicy
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
  return <Renderer artifact={artifact} remoteMediaPolicy={remoteMediaPolicy} />
}

const artifactIcon = (artifact: ArtifactItem) => {
  if (artifact.kind === 'text') return <FileText aria-hidden="true" />
  if (artifact.kind === 'code') return <FileCode2 aria-hidden="true" />
  if (artifact.kind === 'image') return <ImageIcon aria-hidden="true" />
  if (artifact.kind === 'blocked') return <ShieldAlert aria-hidden="true" />
  return <FileQuestion aria-hidden="true" />
}

export function ArtifactRenderer({
  artifact,
  remoteMediaPolicy = 'ask',
}: {
  artifact: ArtifactItem
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
      <RendererBoundary artifact={artifact} remoteMediaPolicy={remoteMediaPolicy} />
    </article>
  )
}

export function ArtifactWorkbench() {
  const [remoteMedia, setRemoteMedia] = useRemoteMediaPreference()

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
            onChange={(event) => setRemoteMedia(event.target.value as typeof remoteMedia)}
          >
            <option value="ask">Ask</option>
            <option value="block">Block</option>
            <option value="allow">Allow</option>
          </select>
        </label>
      </header>
      <p className="artifact-bound-summary">
        {artifactScenario.length} typed records · 4,000-character previews · 0 original bytes
        prefetched
      </p>
      <div className="artifact-list">
        {artifactScenario.map((artifact) => (
          <ArtifactRenderer key={artifact.id} artifact={artifact} remoteMediaPolicy={remoteMedia} />
        ))}
      </div>
    </section>
  )
}
