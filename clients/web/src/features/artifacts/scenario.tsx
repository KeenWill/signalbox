import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createRoot } from 'react-dom/client'
import { Provider } from 'react-redux'
import { AttachmentReferences } from '../../AttachmentReferences'
import { store, useAppSelector } from '../../state'
import { fallbackDescriptor, imageDescriptor, jpegDescriptor } from './artifactScenario'
import '../../app.css'

function OriginalCount() {
  const count = useAppSelector((state) => Object.keys(state.app.originalArtifacts).length)
  return <output aria-label="Original image states">{count}</output>
}

const queryClient = new QueryClient()
const root = document.getElementById('root')
for (const element of [document.documentElement, document.body, root]) {
  if (element) {
    element.style.height = 'auto'
    element.style.overflow = 'visible'
  }
}
if (root)
  createRoot(root).render(
    <Provider store={store}>
      <QueryClientProvider client={queryClient}>
        <main style={{ maxWidth: '50rem', margin: '1rem auto', padding: '1rem' }}>
          <h1>Conversation attachments</h1>
          <p>Here is the image and the trace file.</p>
          <OriginalCount />
          <div style={{ marginTop: location.search.includes('offscreen') ? '200vh' : undefined }}>
            <AttachmentReferences
              attachments={(location.search.includes('original')
                ? [jpegDescriptor]
                : location.search.includes('offscreen') || location.search.includes('label')
                  ? [fallbackDescriptor]
                  : location.search.includes('uppercase')
                    ? [imageDescriptor]
                    : [imageDescriptor, fallbackDescriptor]
              ).map((descriptor) => ({
                blob_id: descriptor.digest,
                length_bytes: descriptor.byte_length,
                media_type: location.search.includes('label')
                  ? new URLSearchParams(location.search).get('label') || 'garbage'
                  : location.search.includes('uppercase')
                    ? descriptor.declared_media_type.toUpperCase()
                    : descriptor.declared_media_type,
              }))}
            />
          </div>
          {location.search.includes('revisit') && <div style={{ height: '200vh' }} />}
        </main>
      </QueryClientProvider>
    </Provider>,
  )
