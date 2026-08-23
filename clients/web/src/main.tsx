import * as Tooltip from '@radix-ui/react-tooltip'
import { HotkeysProvider } from '@tanstack/react-hotkeys'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import {
  createRootRoute,
  createRoute,
  createRouter,
  Navigate,
  Outlet,
  RouterProvider,
} from '@tanstack/react-router'
import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { Provider } from 'react-redux'
import { Workspace } from './App'
import { store } from './state'
import './app.css'

const rootRoute = createRootRoute({ component: () => <Outlet /> })
const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/',
  component: () => (
    <Navigate to="/scenario/$scenarioId" params={{ scenarioId: 'streaming' }} replace />
  ),
})
const scenarioRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/scenario/$scenarioId',
  component: () => <Workspace scenarioId={scenarioRoute.useParams().scenarioId} />,
})
const router = createRouter({ routeTree: rootRoute.addChildren([indexRoute, scenarioRoute]) })
// Tunable effective ceiling: retain recently visited scenario projections without growing the
// development cache for the lifetime of the page.
const QUERY_CACHE_GC_TIME_MS = 5 * 60_000
// Tunable effective ceiling: defer long enough to avoid accidental hover while keeping
// operator-facing explanations responsive.
const TOOLTIP_DELAY_MS = 350
const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, gcTime: QUERY_CACHE_GC_TIME_MS } },
})

declare module '@tanstack/react-router' {
  interface Register {
    router: typeof router
  }
}

const root = document.getElementById('root')
if (!root) throw new Error('Missing web application root')

createRoot(root).render(
  <StrictMode>
    <Provider store={store}>
      <QueryClientProvider client={queryClient}>
        <HotkeysProvider>
          <Tooltip.Provider delayDuration={TOOLTIP_DELAY_MS}>
            <RouterProvider router={router} />
          </Tooltip.Provider>
        </HotkeysProvider>
      </QueryClientProvider>
    </Provider>
  </StrictMode>,
)
