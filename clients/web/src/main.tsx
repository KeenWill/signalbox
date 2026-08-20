import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { Provider } from 'react-redux'
import { HotkeysProvider } from '@tanstack/react-hotkeys'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createRootRoute, createRoute, createRouter, Navigate, Outlet, RouterProvider } from '@tanstack/react-router'
import * as Tooltip from '@radix-ui/react-tooltip'
import { Workspace } from './App'
import { store } from './state'
import './app.css'

const rootRoute = createRootRoute({ component: () => <Outlet /> })
const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/',
  component: () => <Navigate to="/scenario/$scenarioId" params={{ scenarioId: 'streaming' }} replace />,
})
const scenarioRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/scenario/$scenarioId',
  component: () => <Workspace scenarioId={scenarioRoute.useParams().scenarioId} />,
})
const router = createRouter({ routeTree: rootRoute.addChildren([indexRoute, scenarioRoute]) })
const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, gcTime: 5 * 60_000 } },
})

declare module '@tanstack/react-router' {
  interface Register { router: typeof router }
}

const root = document.getElementById('root')
if (!root) throw new Error('Missing web application root')

createRoot(root).render(
  <StrictMode>
    <Provider store={store}>
      <QueryClientProvider client={queryClient}>
        <HotkeysProvider>
          <Tooltip.Provider delayDuration={350}>
            <RouterProvider router={router} />
          </Tooltip.Provider>
        </HotkeysProvider>
      </QueryClientProvider>
    </Provider>
  </StrictMode>,
)
