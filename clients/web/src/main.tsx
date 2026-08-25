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
import { lazy, StrictMode, Suspense } from 'react'
import { createRoot } from 'react-dom/client'
import { Provider } from 'react-redux'
import { ProductApp } from './ProductApp'
import { type ProductRouteId, productRoutes } from './product'
import { selectApp, store } from './state'
import './app.css'

const rootRoute = createRootRoute({ component: () => <Outlet /> })
const ScenarioWorkspace = lazy(() =>
  import('./App').then((module) => ({ default: module.Workspace })),
)
const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/',
  component: () => <Navigate to="/$surface" params={{ surface: 'attention' }} replace />,
})
const productRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/$surface',
  component: () => {
    const candidate = productRoute.useParams().surface
    if (!productRoutes.some((route) => route.id === candidate)) {
      return <Navigate to="/$surface" params={{ surface: 'attention' }} replace />
    }
    return <ProductApp surface={candidate as ProductRouteId} />
  },
})
const scenarioRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/scenario/$scenarioId',
  component: () => (
    <Suspense fallback={<main className="loading">Loading scenario studio…</main>}>
      <ScenarioWorkspace scenarioId={scenarioRoute.useParams().scenarioId} />
    </Suspense>
  ),
})
const router = createRouter({
  routeTree: rootRoute.addChildren([indexRoute, productRoute, scenarioRoute]),
})
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

const initialPresentation = selectApp(store.getState())
document.documentElement.dataset.theme = initialPresentation.theme
document.documentElement.dataset.density = initialPresentation.density

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
