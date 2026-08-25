import * as Tooltip from '@radix-ui/react-tooltip'
import { HotkeysProvider } from '@tanstack/react-hotkeys'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import {
  createRootRoute,
  createRoute,
  createRouter,
  Navigate,
  Outlet,
  parseSearchWith,
  RouterProvider,
  stringifySearchWith,
} from '@tanstack/react-router'
import { lazy, StrictMode, Suspense, useEffect } from 'react'
import { createRoot } from 'react-dom/client'
import { Provider } from 'react-redux'
import { HttpImportApi } from './imports/api'
import { ImportsWorkspace } from './imports/ImportsWorkspace'
import { ScenarioImportApi } from './imports/scenario'
import { ProductApp } from './ProductApp'
import { type ProductRouteId, productRoutes, readProductSearchState } from './product'
import { selectApp, store } from './state'
import './app.css'

const rootRoute = createRootRoute({ component: () => <Outlet /> })
const httpImportApi = new HttpImportApi()
const scenarioImportApi = new ScenarioImportApi()
const ScenarioWorkspace = lazy(() =>
  import('./App').then((module) => ({ default: module.Workspace })),
)
const ScenarioRoute = () => {
  const scenarioId = scenarioRoute.useParams().scenarioId
  useEffect(() => {
    document.title = 'Signalbox scenarios'
  }, [])
  if (scenarioId === 'imports') {
    return <ImportsWorkspace api={scenarioImportApi} scenario />
  }
  return (
    <Suspense fallback={<main className="loading">Loading scenario studio…</main>}>
      <ScenarioWorkspace scenarioId={scenarioId} />
    </Suspense>
  )
}
const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/',
  component: () => <Navigate to="/$surface" params={{ surface: 'attention' }} replace />,
})
const productRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/$surface',
  validateSearch: readProductSearchState,
  component: () => {
    const candidate = productRoute.useParams().surface
    const search = productRoute.useSearch()
    if (!productRoutes.some((route) => route.id === candidate)) {
      return <Navigate to="/$surface" params={{ surface: 'attention' }} replace />
    }
    if (candidate === 'imports') {
      return <ImportsWorkspace api={httpImportApi} scenario={false} />
    }
    return <ProductApp surface={candidate as ProductRouteId} search={search} />
  },
})
const scenarioRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/scenario/$scenarioId',
  component: ScenarioRoute,
})
const router = createRouter({
  routeTree: rootRoute.addChildren([indexRoute, productRoute, scenarioRoute]),
  parseSearch: parseSearchWith((value) => value),
  stringifySearch: stringifySearchWith(String),
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
