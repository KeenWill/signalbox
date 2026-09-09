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
import {
  type ComponentType,
  type KeyboardEvent,
  type ReactNode,
  useEffect,
  useRef,
  useState,
} from 'react'
import { type CommandContext, invokeCommand } from '../../commands'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { enumLabel } from '../../labels'
import { actions, useAppDispatch, useAppSelector } from '../../state'
import {
  artifactScenario,
  selectBoundedOriginalView,
  selectProvenViewDerivation,
} from './artifactScenario'
import {
  ARTIFACT_PREVIEW_CHARACTERS,
  type ArtifactItem,
  boundArtifactText,
  type CodeArtifact,
  type DerivativeArtifact,
  type DocumentArtifact,
  type GenericBlobArtifact,
  type MediaPlaceholderArtifact,
  type RemoteImageArtifact,
  type RenderableArtifact,
  type SignalboxImageArtifact,
  type TextArtifact,
} from './artifactTypes'
import { useVerifiedDerivedImage } from './derivedImageService'
import { useVerifiedOriginalImage } from './originalImageService'
import { admitRemoteMediaUrl } from './remoteMediaPreference'
import './artifacts.css'

type WebBlobAvailableView = WebBlobDescriptor['available_views'][number]
type WebBlobViewKind = WebBlobAvailableView['kind']
type SupportedArtifactKind = RenderableArtifact['kind']

const IMAGE_VIEW_PRIORITY: ReadonlyArray<WebBlobViewKind> = ['preview', 'thumbnail']

export const selectImageView = (
  descriptor: WebBlobDescriptor,
  failedContentUrls: ReadonlySet<string> = new Set(),
): WebBlobAvailableView | undefined =>
  IMAGE_VIEW_PRIORITY.map((kind) =>
    descriptor.available_views.find((view) => view.kind === kind),
  ).find((view) => view !== undefined && !failedContentUrls.has(view.content_url))

export const imageViewLabel = (kind: WebBlobViewKind): string =>
  ({
    browser_native: 'Original',
    preview: 'Preview',
    thumbnail: 'Thumbnail',
    download: 'Download',
  })[kind]

export const selectBlobView = (
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

const scrollArtifactPreviewByPage = (event: KeyboardEvent<HTMLTextAreaElement>) => {
  if (event.key !== 'PageDown' && event.key !== 'PageUp') return
  event.preventDefault()
  const direction = event.key === 'PageDown' ? 1 : -1
  event.currentTarget.scrollBy({ top: direction * event.currentTarget.clientHeight })
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
        aria-label={`Preview of ${artifact.displayName}`}
        onFocusCapture={() => selectArtifact(commandContext, artifact.id)}
        onKeyDown={scrollArtifactPreviewByPage}
        readOnly
        value={bounded.content}
      />
      <BoundedFooter
        sourceComplete={artifact.sourceComplete}
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
        aria-label={`Preview of ${artifact.displayName}`}
        onFocusCapture={() => selectArtifact(commandContext, artifact.id)}
        onKeyDown={scrollArtifactPreviewByPage}
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
  sourceComplete = true,
  omittedCharacters,
  canExpand,
  expanded,
  onToggle,
}: {
  sourceComplete?: boolean
  omittedCharacters: number
  canExpand: boolean
  expanded: boolean
  onToggle: () => void
}) {
  if (sourceComplete && omittedCharacters === 0 && !canExpand && !expanded) return null

  return (
    <footer className="artifact-bounded-footer">
      <span>
        {!sourceComplete
          ? 'Partial text'
          : omittedCharacters > 0
            ? `${omittedCharacters.toLocaleString()} characters omitted`
            : null}
      </span>
      {(canExpand || expanded) && (
        <button type="button" onClick={onToggle}>
          {expanded ? <Minimize2 aria-hidden="true" /> : <Maximize2 aria-hidden="true" />}
          {expanded ? 'Collapse preview' : 'Expand preview'}
        </button>
      )}
    </footer>
  )
}

function SignalboxImageBody({ artifact, commandContext }: RendererProps<SignalboxImageArtifact>) {
  const dispatch = useAppDispatch()
  const [failedAutomaticUrls, setFailedAutomaticUrls] = useState<ReadonlySet<string>>(
    () => new Set(),
  )
  const originalState = useAppSelector((state) => state.app.originalArtifacts[artifact.id])
  const { descriptor } = artifact.source
  const automatic = selectImageView(descriptor, failedAutomaticUrls)
  const original = selectBoundedOriginalView(descriptor)
  const download = selectBlobView(descriptor, 'download')
  const originalRequested = originalState === 'loading' || originalState === 'loaded'
  const originalQuery = useVerifiedOriginalImage(original, originalRequested)
  const [verifiedOriginalUrl, setVerifiedOriginalUrl] = useState<string | null>(null)
  const verifiedBlob = originalQuery.data
  const candidate =
    originalRequested && original && verifiedOriginalUrl !== null ? original : automatic
  const derivation = candidate ? selectProvenViewDerivation(descriptor, candidate) : undefined
  const rendered =
    candidate &&
    ((candidate.kind !== 'preview' && candidate.kind !== 'thumbnail') || derivation !== undefined)
      ? candidate
      : undefined

  const derivedQuery = useVerifiedDerivedImage(
    rendered?.kind === 'preview' || rendered?.kind === 'thumbnail' ? rendered : undefined,
  )
  const derivedFailed = derivedQuery.isError
  const automaticUrl = automatic?.content_url
  useEffect(() => {
    if (!derivedFailed || automaticUrl === undefined) return
    setFailedAutomaticUrls((current) => new Set([...current, automaticUrl]))
  }, [derivedFailed, automaticUrl])
  const renderedUrl = rendered?.kind === 'browser_native' ? verifiedOriginalUrl : derivedQuery.url

  // The object URL is allocated and revoked by one effect owning its lifecycle: allocation during
  // render would strand the extra URL produced by development double-rendering unrevoked.
  useEffect(() => {
    if (verifiedBlob === undefined) return
    const url = URL.createObjectURL(verifiedBlob)
    setVerifiedOriginalUrl(url)
    return () => {
      setVerifiedOriginalUrl(null)
      URL.revokeObjectURL(url)
    }
  }, [verifiedBlob])

  // Project the service's request state into the explicit control state: a new failure settles the
  // request as failed, and a retry that finds a failure this projection already settled asks the
  // service to fetch again. Errors settled before this mount (the ref's initial value) stay
  // retryable rather than immediately re-settling.
  const settledErrorCount = useRef(originalQuery.errorUpdateCount)
  const { errorUpdateCount, isError, isFetching, refetch } = originalQuery
  const verified = originalQuery.data !== undefined
  useEffect(() => {
    if (!originalRequested || isFetching || verified) return
    if (errorUpdateCount > settledErrorCount.current) {
      settledErrorCount.current = errorUpdateCount
      dispatch(actions.artifactOriginalSettled({ id: artifact.id, result: 'failed' }))
      return
    }
    if (isError) void refetch()
  }, [
    artifact.id,
    dispatch,
    errorUpdateCount,
    isError,
    isFetching,
    originalRequested,
    refetch,
    verified,
  ])

  return (
    <div className="artifact-image-layout">
      <div className="artifact-visual">
        {rendered && renderedUrl ? (
          <img
            src={renderedUrl}
            alt={`${imageViewLabel(rendered.kind)} of ${artifact.displayName}`}
            loading="lazy"
            onLoad={() => {
              if (rendered.kind === 'browser_native' && originalState === 'loading') {
                dispatch(actions.artifactOriginalSettled({ id: artifact.id, result: 'loaded' }))
              }
            }}
            onError={() => {
              if (rendered.kind === 'browser_native') {
                setVerifiedOriginalUrl(null)
                originalQuery.discard()
                dispatch(actions.artifactOriginalSettled({ id: artifact.id, result: 'failed' }))
              } else {
                setFailedAutomaticUrls((current) => {
                  const next = new Set(current)
                  next.add(rendered.content_url)
                  return next
                })
              }
            }}
          />
        ) : (
          <FileQuestion aria-label="No preview available" />
        )}
      </div>
      <ArtifactMetadata
        renderer={rendered ? enumLabel(rendered.kind) : 'Details only'}
        mediaType={descriptor.declared_media_type}
        byteLength={descriptor.byte_length}
        provenance={derivation?.transformation_name ?? 'Original content'}
      >
        {original && (
          <button
            type="button"
            aria-pressed={originalState === 'loaded'}
            aria-disabled={originalState === 'loading' || originalState === 'loaded'}
            onClick={() => {
              if (originalState !== 'loading' && originalState !== 'loaded') {
                invokeArtifactAction(commandContext, 'artifact.original.load', artifact.id)
              }
            }}
          >
            <Maximize2 aria-hidden="true" />
            {originalState === 'loading'
              ? 'Loading original'
              : originalState === 'loaded'
                ? 'Original loaded'
                : originalState === 'failed'
                  ? 'Retry original'
                  : 'Load original'}
          </button>
        )}
        {originalState === 'failed' && <p role="status">Original image failed to load</p>}
        {originalState !== 'failed' &&
          !originalRequested &&
          failedAutomaticUrls.size > 0 &&
          !automatic && <p role="status">Preview unavailable</p>}
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
        renderer={admittedUrl === null ? 'Remote media blocked' : 'Remote media unavailable'}
        mediaType="Not inspected"
        provenance="External URL"
      ></ArtifactMetadata>
    </div>
  )
}

function GenericBlobBody({ artifact }: RendererProps<GenericBlobArtifact>) {
  const download = selectBlobView(artifact.descriptor, 'download')

  return (
    <div className="artifact-image-layout">
      <div className="artifact-visual">
        <FileQuestion aria-label="No preview available" />
      </div>
      <ArtifactMetadata
        renderer="Details only"
        mediaType={artifact.descriptor.declared_media_type}
        byteLength={artifact.descriptor.byte_length}
        provenance="Original content"
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

function DocumentBody({ artifact }: RendererProps<DocumentArtifact>) {
  const { descriptor } = artifact.source
  const browserNative = selectBlobView(descriptor, 'browser_native')
  const download = selectBlobView(descriptor, 'download')

  return (
    <div className="artifact-placeholder-layout">
      <div className="artifact-document-placeholder">
        <File aria-hidden="true" />
        <strong>{artifact.documentKind === 'pdf' ? 'PDF document' : 'Document'}</strong>
      </div>
      <ArtifactMetadata
        renderer="Document"
        mediaType={descriptor.declared_media_type}
        provenance="Original content"
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
  const [failedContentUrl, setFailedContentUrl] = useState<string | null>(null)
  const rendered = selectBlobView(artifact.source.descriptor, artifact.viewKind)
  const derivation = rendered
    ? selectProvenViewDerivation(artifact.source.descriptor, rendered)
    : undefined
  const verified = useVerifiedDerivedImage(derivation ? rendered : undefined)
  const loadFailed = rendered?.content_url === failedContentUrl || verified.isError

  if (!rendered || !derivation || loadFailed) {
    return (
      <div className="artifact-state blocked" role="status">
        <ShieldAlert aria-hidden="true" />
        <div>
          <strong>Preview unavailable</strong>
        </div>
      </div>
    )
  }

  return (
    <div className="artifact-image-layout">
      <div className="artifact-visual">
        {verified.url && (
          <img
            src={verified.url}
            alt={`${enumLabel(artifact.viewKind)} of ${artifact.displayName}`}
            loading="lazy"
            onError={() => setFailedContentUrl(rendered.content_url)}
          />
        )}
      </div>
      <ArtifactMetadata
        renderer={enumLabel(artifact.viewKind)}
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
      </div>
      <ArtifactMetadata
        renderer={enumLabel(artifact.mediaKind)}
        mediaType={descriptor.declared_media_type}
        provenance="Original content"
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
  byteLength,
  provenance,
  children,
}: {
  renderer: string
  mediaType: string
  byteLength?: string
  provenance: string
  children?: ReactNode
}) {
  return (
    <div className="artifact-detail">
      <dl>
        <div>
          <dt>Shown as</dt>
          <dd>{renderer}</dd>
        </div>
        <div>
          <dt>Type (as declared)</dt>
          <dd>{mediaType}</dd>
        </div>
        {byteLength !== undefined && (
          <div>
            <dt>Size</dt>
            <dd>{BigInt(byteLength).toLocaleString()} bytes</dd>
          </div>
        )}
        <div>
          <dt>Source</dt>
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
  document: DocumentBody,
  derivative: DerivativeBody,
  media_placeholder: MediaPlaceholderBody,
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
          <strong>Preview unavailable</strong>
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
          <small>
            {enumLabel(artifact.kind === 'blocked' ? artifact.attemptedKind : artifact.kind)}
          </small>
        </div>
      </button>
      <RendererBoundary artifact={artifact} commandContext={commandContext} />
    </article>
  )
}

export function ArtifactWorkbench({ commandContext }: { commandContext: CommandContext }) {
  return (
    <section
      className="artifact-panel"
      aria-labelledby="artifact-heading"
      data-command-focus-target
      tabIndex={-1}
    >
      <header className="section-header artifact-panel-heading">
        <div>
          <h1 id="artifact-heading">Artifacts</h1>
        </div>
      </header>
      <p className="artifact-bound-summary">
        {artifactScenario.length} artifacts · {ARTIFACT_PREVIEW_CHARACTERS.toLocaleString()}
        -character previews · Originals load on request
      </p>
      <div className="artifact-list">
        {artifactScenario.map((artifact) => (
          <ArtifactRenderer key={artifact.id} artifact={artifact} commandContext={commandContext} />
        ))}
      </div>
    </section>
  )
}
