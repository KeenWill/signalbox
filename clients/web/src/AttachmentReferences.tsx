import * as Dialog from '@radix-ui/react-dialog'
import { useEffect, useMemo, useRef, useState } from 'react'
import { useStore } from 'react-redux'
import {
  ArtifactInspector,
  artifactResolutionId,
  attachmentTypeLabel,
  emptyArtifactInspectorState,
  inspectedArtifact,
  nextResolutionSequence,
} from './ArtifactInspector'
import { type CommandContext, invokeCommand } from './commands'
import { ArtifactRenderer, selectBlobView } from './features/artifacts/ArtifactRenderer'
import { selectBoundedOriginalView } from './features/artifacts/artifactScenario'
import { useArtifactDescriptor, useBlobCapability } from './features/artifacts/descriptorService'
import type {
  WebSessionTimelineDetailBody,
  WebTimelineBlobReference,
} from './generated/web-contract.mjs'
import { actions, type store as appStore } from './state'
import './features/artifacts/artifacts.css'

const useAttachmentStore = useStore.withTypes<typeof appStore>()

type Attachments = Extract<WebSessionTimelineDetailBody, { type: 'user_input' }>['attachments']

function AttachmentReference({
  attachment,
  available,
}: {
  attachment: WebTimelineBlobReference
  available: boolean
}) {
  const container = useRef<HTMLDivElement>(null)
  const chip = useRef<HTMLButtonElement>(null)
  const [visible, setVisible] = useState(false)
  const [reservedHeight, setReservedHeight] = useState(0)
  useEffect(() => {
    const element = container.current
    if (!element) return
    const observer = new IntersectionObserver(([entry]) => {
      if (!entry) return
      if (!entry.isIntersecting) setReservedHeight(entry.boundingClientRect.height)
      setVisible(entry.isIntersecting)
    })
    observer.observe(element)
    return () => observer.disconnect()
  }, [])
  const store = useAttachmentStore()
  const [sequence] = useState(nextResolutionSequence)
  const [state, setState] = useState(emptyArtifactInspectorState)
  const input = useMemo(
    () => ({
      digest: attachment.blob_id,
      mediaType: attachment.media_type ?? 'application/octet-stream',
    }),
    [attachment.blob_id, attachment.media_type],
  )
  const descriptor = useArtifactDescriptor(available && visible ? input : null)
  const artifact = useMemo(
    () => (descriptor.data ? inspectedArtifact(descriptor.data, sequence) : null),
    [descriptor.data, sequence],
  )
  const inlineId = artifact?.id
  const detailId = state.request ? artifactResolutionId(state.request) : undefined
  useEffect(() => {
    if (!inlineId) return
    return () => {
      store.dispatch(actions.artifactOriginalReleased(inlineId))
    }
  }, [inlineId, store])
  useEffect(() => {
    if (!detailId) return
    return () => {
      store.dispatch(actions.artifactOriginalReleased(detailId))
    }
  }, [detailId, store])
  const name = artifact?.displayName ?? attachmentTypeLabel(attachment.media_type)
  const download = descriptor.data ? selectBlobView(descriptor.data, 'download') : undefined
  const context: CommandContext = {
    dispatch: store.dispatch,
    getState: store.getState,
    timelineIds: [],
    artifactPreviewIds: [],
    artifactOriginalIds:
      artifact?.kind === 'image' &&
      artifact.source.kind === 'signalbox_blob' &&
      selectBoundedOriginalView(artifact.source.descriptor)
        ? [artifact.id]
        : [],
    focusTimeline: () => {},
    openArtifactInspector: () =>
      setState({
        digest: input.digest,
        mediaType: input.mediaType,
        displayFilename: '',
        request: { ...input, sequence: nextResolutionSequence() },
      }),
  }
  const close = () => setState(emptyArtifactInspectorState)
  return (
    <Dialog.Root
      open={state.request !== null}
      onOpenChange={(open) => {
        if (!open) close()
      }}
    >
      <div
        ref={container}
        className="inline-attachment"
        style={{ minHeight: visible ? undefined : reservedHeight }}
      >
        <div className="inline-attachment-chip">
          <Dialog.Trigger asChild>
            <button
              type="button"
              ref={chip}
              disabled={!available}
              onClick={() => invokeCommand('artifact.open', context)}
            >
              {name} · {attachment.media_type ?? 'Unknown file type'} ·{' '}
              {BigInt(attachment.length_bytes).toLocaleString()} bytes
            </button>
          </Dialog.Trigger>
          {download && (
            <a href={download.content_url} download={name}>
              Download
            </a>
          )}
        </div>
        {artifact?.kind === 'image' && (
          <ArtifactRenderer
            artifact={artifact}
            commandContext={context}
            onInspect={() => invokeCommand('artifact.open', context)}
          />
        )}
        {available && visible && descriptor.isPending && (
          <small role="status">Loading attachment…</small>
        )}
        {descriptor.isError && (
          <p role="status">
            Attachment unavailable.{' '}
            <button
              type="button"
              onClick={(event) => {
                const opener = event.currentTarget
                const wasFocused = document.activeElement === opener
                void descriptor.refetch().then((result) => {
                  if (!result.isSuccess || !wasFocused) return
                  requestAnimationFrame(() => {
                    if (
                      document.activeElement === opener ||
                      (!opener.isConnected && document.activeElement === document.body)
                    )
                      chip.current?.focus()
                  })
                })
              }}
            >
              Retry attachment
            </button>
          </p>
        )}
      </div>
      <Dialog.Portal>
        <Dialog.Overlay className="attachment-detail-overlay" />
        <Dialog.Content className="attachment-detail-pane" aria-describedby={undefined}>
          <Dialog.Title className="sr-only">Attachment details</Dialog.Title>
          <ArtifactInspector
            available={available}
            commandContext={context}
            state={state}
            onStateChange={setState}
            onClose={close}
          />
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  )
}

export function AttachmentReferences({ attachments }: { attachments: Attachments }) {
  const available = useBlobCapability(attachments.length > 0)
  if (attachments.length === 0) return null
  return (
    <ul className="inline-attachments" aria-label="Attachments">
      {attachments.map((attachment, index) => (
        // biome-ignore lint/suspicious/noArrayIndexKey: Attachment positions are immutable; repeated digests are valid.
        <li key={`${attachment.blob_id}:${index}`}>
          <AttachmentReference attachment={attachment} available={available} />
        </li>
      ))}
    </ul>
  )
}
