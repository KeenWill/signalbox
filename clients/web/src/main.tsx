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
import { Component, lazy, type ReactNode, StrictMode, Suspense, useMemo, useState } from 'react'
import { createRoot } from 'react-dom/client'
import { Provider } from 'react-redux'
import { ProductApp } from './ProductApp'
import { applyPresentationPreferences } from './preferences'
import { type ProductRouteId, productRoutes } from './product'
import { selectApp, store } from './state'
import './app.css'

const rootRoute = createRootRoute({ component: () => <Outlet /> })

const createScenarioWorkspace = (_attempt: number) =>
  lazy(() => import('./App').then((module) => ({ default: module.Workspace })))

class ScenarioChunkBoundary extends Component<
  { children: ReactNode; onRetry: () => void },
  { failed: boolean }
> {
  state = { failed: false }

  static getDerivedStateFromError() {
    return { failed: true }
  }

  render() {
    if (this.state.failed) {
      return (
        <main className="loading">
          <p>Scenario studio could not be loaded.</p>
          <button type="button" onClick={this.props.onRetry}>
            Retry scenario studio
          </button>
        </main>
      )
    }
    return this.props.children
  }
}

function ScenarioRoute() {
  const scenarioId = scenarioRoute.useParams().scenarioId
  const [attempt, setAttempt] = useState(0)
  const ScenarioWorkspace = useMemo(() => createScenarioWorkspace(attempt), [attempt])
  return (
    <ScenarioChunkBoundary key={attempt} onRetry={() => setAttempt((value) => value + 1)}>
      <Suspense fallback={<main className="loading">Loading scenario studio…</main>}>
        <ScenarioWorkspace scenarioId={scenarioId} />
      </Suspense>
    </ScenarioChunkBoundary>
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
  component: ScenarioRoute,
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

applyPresentationPreferences(selectApp(store.getState()))

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
