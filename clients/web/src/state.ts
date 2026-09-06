import { configureStore, createSlice, type Middleware } from '@reduxjs/toolkit'
import { useDispatch, useSelector } from 'react-redux'
import type { AttentionSyncPhase } from './attention'
import {
  type BrowserPreferences,
  createDefaultBrowserPreferences,
  isBoundedLogicalPosition,
  loadBrowserPreferences,
  MAX_SAVED_LOGICAL_POSITIONS,
  saveBrowserPreferences,
  serializeBrowserPreferences,
} from './preferences'

export type LayoutMode = 'focus' | 'workbench'
export type DensityMode = 'compact' | 'comfortable'
export type DetailMode = 'full' | 'condensed' | 'results'
export type ThemeMode = 'light' | 'dark'
export type Overlay = 'palette' | 'help' | 'navigation' | null
export type ArtifactOriginalState = 'loading' | 'loaded' | 'failed'

export interface VisibleRange {
  start: number
  end: number
}

interface AppState extends BrowserPreferences {
  layout: LayoutMode
  density: DensityMode
  detail: DetailMode
  theme: ThemeMode
  overlay: Overlay
  attentionSync: AttentionSyncPhase
  selectedTimeline: string | null
  selectedArtifact: string | null
  expandedArtifacts: Record<string, boolean>
  originalArtifacts: Record<string, ArtifactOriginalState>
  transcriptRange: VisibleRange
  tableRange: VisibleRange
  activitySequence: number
}

const initialState: AppState = {
  ...loadBrowserPreferences(),
  overlay: null,
  attentionSync: 'idle',
  selectedTimeline: null,
  selectedArtifact: null,
  expandedArtifacts: {},
  originalArtifacts: {},
  transcriptRange: { start: 0, end: 0 },
  tableRange: { start: 0, end: 0 },
  activitySequence: 0,
}

// Tunable effective ceiling: diagnostics retain a concise Redux activity tail for local triage.
const RECENT_REDUX_ACTIONS = 18
// Tunable effective ceiling: Redux DevTools keeps a practical local interaction history.
const REDUX_DEVTOOLS_ACTIONS = 24

const appSlice = createSlice({
  name: 'app',
  initialState,
  reducers: {
    layoutSet(state, action: { payload: LayoutMode }) {
      state.layout = action.payload
      state.activitySequence += 1
    },
    densitySet(state, action: { payload: DensityMode }) {
      state.density = action.payload
      state.activitySequence += 1
    },
    detailSet(state, action: { payload: DetailMode }) {
      state.detail = action.payload
      state.activitySequence += 1
    },
    themeSet(state, action: { payload: ThemeMode }) {
      state.theme = action.payload
      state.activitySequence += 1
    },
    paneSizesSet(state, action: { payload: BrowserPreferences['paneSizes'] }) {
      state.paneSizes = action.payload
      state.activitySequence += 1
    },
    paneSizesPreviewed(state, action: { payload: BrowserPreferences['paneSizes'] }) {
      state.paneSizes = action.payload
    },
    preferencesReset(state) {
      Object.assign(state, createDefaultBrowserPreferences())
      state.activitySequence += 1
    },
    logicalPositionRecorded(state, action: { payload: { sessionId: string; position: string } }) {
      if (!isBoundedLogicalPosition(action.payload.sessionId, action.payload.position)) return
      const nextPositions = { ...state.lastLogicalPositions }
      delete nextPositions[action.payload.sessionId]
      nextPositions[action.payload.sessionId] = action.payload.position
      const lastLogicalPositions = Object.fromEntries(
        Object.entries(nextPositions).slice(-MAX_SAVED_LOGICAL_POSITIONS),
      )
      if (
        serializeBrowserPreferences({
          layout: state.layout,
          density: state.density,
          detail: state.detail,
          theme: state.theme,
          paneSizes: state.paneSizes,
          lastLogicalPositions,
        }) === null
      ) {
        return
      }
      state.lastLogicalPositions = lastLogicalPositions
    },
    overlaySet(state, action: { payload: Overlay }) {
      state.overlay = action.payload
      state.activitySequence += 1
    },
    attentionSyncSet(state, action: { payload: AttentionSyncPhase }) {
      if (state.attentionSync === action.payload) return
      state.attentionSync = action.payload
      state.activitySequence += 1
    },
    timelineSelected(state, action: { payload: string | null }) {
      state.selectedTimeline = action.payload
      state.activitySequence += 1
    },
    artifactSelected(state, action: { payload: string | null }) {
      state.selectedArtifact = action.payload
      state.activitySequence += 1
    },
    artifactExpansionSet(state, action: { payload: { id: string; expanded: boolean } }) {
      state.expandedArtifacts[action.payload.id] = action.payload.expanded
      state.activitySequence += 1
    },
    artifactOriginalRequested(state, action: { payload: string }) {
      state.originalArtifacts[action.payload] = 'loading'
      state.activitySequence += 1
    },
    artifactOriginalSettled(
      state,
      action: { payload: { id: string; result: 'loaded' | 'failed' } },
    ) {
      state.originalArtifacts[action.payload.id] = action.payload.result
      state.activitySequence += 1
    },
    transcriptRangeSet(state, action: { payload: VisibleRange }) {
      state.transcriptRange = action.payload
    },
    tableRangeSet(state, action: { payload: VisibleRange }) {
      state.tableRange = action.payload
    },
  },
})

const actionTrace: string[] = []
const telemetryActionTypes = new Set<string>([
  appSlice.actions.transcriptRangeSet.type,
  appSlice.actions.tableRangeSet.type,
])
const traceMiddleware: Middleware = () => (next) => (action) => {
  if (typeof action === 'object' && action !== null && 'type' in action) {
    const actionType = String(action.type)
    if (telemetryActionTypes.has(actionType)) return next(action)
    actionTrace.push(actionType)
    if (actionTrace.length > RECENT_REDUX_ACTIONS) actionTrace.shift()
  }
  return next(action)
}

const preferenceActionTypes = new Set<string>([
  appSlice.actions.layoutSet.type,
  appSlice.actions.densitySet.type,
  appSlice.actions.detailSet.type,
  appSlice.actions.themeSet.type,
  appSlice.actions.paneSizesSet.type,
  appSlice.actions.preferencesReset.type,
  appSlice.actions.logicalPositionRecorded.type,
])
const preferenceMiddleware: Middleware = (api) => (next) => (action) => {
  const result = next(action)
  if (
    typeof action === 'object' &&
    action !== null &&
    'type' in action &&
    preferenceActionTypes.has(String(action.type))
  ) {
    const app = (api.getState() as { app: AppState }).app
    saveBrowserPreferences({
      layout: app.layout,
      density: app.density,
      detail: app.detail,
      theme: app.theme,
      paneSizes: app.paneSizes,
      lastLogicalPositions: app.lastLogicalPositions,
    })
  }
  return result
}

export const createAppStore = () =>
  configureStore({
    reducer: { app: appSlice.reducer },
    middleware: (getDefaultMiddleware) =>
      getDefaultMiddleware().concat(traceMiddleware, preferenceMiddleware),
    devTools: import.meta.env.DEV ? { maxAge: REDUX_DEVTOOLS_ACTIONS, trace: false } : false,
  })

export const store = createAppStore()

export const actions = appSlice.actions
export type RootState = ReturnType<typeof store.getState>
export type AppDispatch = typeof store.dispatch
export const selectApp = (state: RootState) => state.app
export const getRecentActions = (): readonly string[] => actionTrace
export const useAppDispatch = useDispatch.withTypes<AppDispatch>()
export const useAppSelector = useSelector.withTypes<RootState>()
