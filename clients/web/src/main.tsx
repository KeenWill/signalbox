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
import { HttpImportApi } from './imports/api'
import { ImportsWorkspace } from './imports/ImportsWorkspace'
import { ScenarioImportApi } from './imports/scenario'
import { ProductApp } from './ProductApp'
import { type ProductRouteId, productRoutes } from './product'
import { defaultSearchUsageRouteState, type SearchUsageRouteState } from './SearchUsage'
import { selectApp, store } from './state'
import './app.css'

const rootRoute = createRootRoute({ component: () => <Outlet /> })
const httpImportApi = new HttpImportApi()
const scenarioImportApi = new ScenarioImportApi()
const ScenarioWorkspace = lazy(() =>
  import('./App').then((module) => ({ default: module.Workspace })),
)

const routeString = (value: unknown): string | undefined =>
  typeof value === 'string' ? value : undefined

// A sparse URL must not report absent parameters as explicit `undefined`: the scenario screen
// spreads this result over `defaultSearchUsageRouteState`, so a present-but-undefined property
// would overwrite its default instead of falling back to it.
const withoutAbsent = (state: Partial<SearchUsageRouteState>): Partial<SearchUsageRouteState> =>
  Object.fromEntries(
    Object.entries(state).filter(([, value]) => value !== undefined),
  ) as Partial<SearchUsageRouteState>

const validateScenarioSearch = (
  search: Record<string, unknown>,
): Partial<SearchUsageRouteState> => {
  const view = routeString(search.view)
  const searchScope = routeString(search.searchScope)
  const usageSession = routeString(search.usageSession)
  const provenance = routeString(search.provenance)
  const callKind = routeString(search.callKind)
  return withoutAbsent({
    view: view === 'usage' ? 'usage' : view === 'search' ? 'search' : undefined,
    q: routeString(search.q),
    searchScope:
      searchScope === 'session' ? 'session' : searchScope === 'global' ? 'global' : undefined,
    usageSession:
      usageSession === 'current' ? 'current' : usageSession === 'all' ? 'all' : undefined,
    provenance:
      provenance === 'reported' ? 'reported' : provenance === 'estimated' ? 'estimated' : undefined,
    modelId: routeString(search.modelId),
    callKind:
      callKind === 'model_call' ||
      callKind === 'approval_judge' ||
      callKind === 'context_compaction'
        ? callKind
        : undefined,
  })
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
    if (candidate === 'imports') {
      return <ImportsWorkspace api={httpImportApi} scenario={false} />
    }
    return <ProductApp surface={candidate as ProductRouteId} />
  },
})
const scenarioRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/scenario/$scenarioId',
  validateSearch: validateScenarioSearch,
  component: ScenarioScreen,
})

function ScenarioScreen() {
  const { scenarioId } = scenarioRoute.useParams()
  const search = scenarioRoute.useSearch()
  const navigate = scenarioRoute.useNavigate()
  const route = { ...defaultSearchUsageRouteState, ...search }
  if (scenarioId === 'imports') {
    return <ImportsWorkspace api={scenarioImportApi} scenario />
  }
  return (
    <Suspense fallback={<main className="loading">Loading scenario studio…</main>}>
      <ScenarioWorkspace
        key={scenarioId}
        scenarioId={scenarioId}
        route={route}
        onRouteChange={(patch) =>
          void navigate({ search: (previous) => ({ ...previous, ...patch }), replace: true })
        }
      />
    </Suspense>
  )
}
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
