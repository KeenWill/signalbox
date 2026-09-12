import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createRootRoute, createRouter, RouterProvider } from '@tanstack/react-router'
import { createRoot } from 'react-dom/client'
import { Provider } from 'react-redux'
import { webContractBootstrapFixture } from '../product.fixture'
import { SessionTranscriptText } from '../SessionTranscriptText'
import { store } from '../state'
import { transcriptSessionId } from './transcript.fixture'
import '../app.css'

const queries = new QueryClient()
const params = new URLSearchParams(window.location.search)
const router = createRouter({
  routeTree: createRootRoute({
    component: () => (
      <Provider store={store}>
        <QueryClientProvider client={queries}>
          <main style={{ maxWidth: '60rem', margin: '1rem auto' }}>
            <SessionTranscriptText
              sessionId={transcriptSessionId}
              first="1"
              through="100000"
              observed="100000"
              limits={webContractBootstrapFixture.limits}
              eventSequence={params.get('around') ?? undefined}
              turnId={params.get('turn') ?? undefined}
            />
          </main>
        </QueryClientProvider>
      </Provider>
    ),
  }),
})
const element = document.getElementById('root')
if (element) createRoot(element).render(<RouterProvider router={router} />)
