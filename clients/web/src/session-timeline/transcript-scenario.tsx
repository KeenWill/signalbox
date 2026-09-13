import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createRootRoute, createRouter, RouterProvider } from '@tanstack/react-router'
import { useCallback, useEffect, useRef, useState } from 'react'
import { createRoot } from 'react-dom/client'
import { Provider } from 'react-redux'
import { invokeCommand } from '../commands'
import { webContractBootstrapFixture } from '../product.fixture'
import { SessionTranscriptText } from '../SessionTranscriptText'
import { store } from '../state'
import { transcriptSessionId } from './transcript.fixture'
import '../app.css'

const queries = new QueryClient()
const params = new URLSearchParams(window.location.search)
const router = createRouter({
  routeTree: createRootRoute({
    component: Scenario,
  }),
})

function Scenario() {
  const [observed, setObserved] = useState(100000)
  const unwind = useRef<(() => boolean) | null>(null)
  const registerUnwind = useCallback((handler: () => boolean) => {
    unwind.current = handler
    return () => {
      unwind.current = null
    }
  }, [])
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return
      invokeCommand('surface.escape', {
        dispatch: store.dispatch,
        getState: store.getState,
        timelineIds: [],
        artifactPreviewIds: [],
        artifactOriginalIds: [],
        focusTimeline: () => {},
        unwindSurface: () => unwind.current?.() ?? false,
      })
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])
  return (
    <Provider store={store}>
      <QueryClientProvider client={queries}>
        <main style={{ maxWidth: '60rem', margin: '1rem auto' }}>
          <SessionTranscriptText
            sessionId={transcriptSessionId}
            first="1"
            through="100000"
            observed={String(observed)}
            limits={webContractBootstrapFixture.limits}
            registerUnwind={registerUnwind}
            eventSequence={params.get('around') ?? undefined}
            turnId={params.get('turn') ?? undefined}
            renderTool={
              params.has('renderer')
                ? (tool, detail) => (
                    <section aria-label="Injected tool renderer" data-detail={detail}>
                      <p>{tool.arguments?.text}</p>
                      {tool.evidence.type === 'physical_attempt' && (
                        <p>{tool.evidence.result?.text}</p>
                      )}
                    </section>
                  )
                : undefined
            }
          />
          {params.has('live') && (
            <button type="button" onClick={() => setObserved((value) => value + 1)}>
              Advance transcript
            </button>
          )}
        </main>
      </QueryClientProvider>
    </Provider>
  )
}
const element = document.getElementById('root')
if (element) createRoot(element).render(<RouterProvider router={router} />)
