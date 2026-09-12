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
                : location.search.includes('offscreen')
                  ? [fallbackDescriptor]
                  : [imageDescriptor, fallbackDescriptor]
              ).map((descriptor) => ({
                blob_id: descriptor.digest,
                length_bytes: descriptor.byte_length,
                media_type: descriptor.declared_media_type,
              }))}
            />
          </div>
        </main>
      </QueryClientProvider>
    </Provider>,
  )
