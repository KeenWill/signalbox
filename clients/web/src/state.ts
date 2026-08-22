import { configureStore, createSlice, type Middleware } from '@reduxjs/toolkit'
import { useDispatch, useSelector } from 'react-redux'
import {
  type BrowserPreferences,
  createDefaultBrowserPreferences,
  loadBrowserPreferences,
  MAX_SAVED_LOGICAL_POSITIONS,
  type RemoteMediaPolicy,
  saveBrowserPreferences,
} from './preferences'

export type LayoutMode = 'focus' | 'workbench'
export type DensityMode = 'compact' | 'comfortable'
export type DetailMode = 'full' | 'condensed' | 'results'
export type ThemeMode = 'light' | 'dark'
export type Overlay = 'palette' | 'help' | 'navigation' | null

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
  selectedTimeline: string | null
  selectedArtifact: string | null
  expandedArtifacts: Record<string, boolean>
  originalArtifacts: Record<string, boolean>
  transcriptRange: VisibleRange
  tableRange: VisibleRange
  activitySequence: number
}

const initialState: AppState = {
  ...loadBrowserPreferences(),
  overlay: null,
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
    remoteMediaSet(state, action: { payload: RemoteMediaPolicy }) {
      state.remoteMedia = action.payload
      state.activitySequence += 1
    },
    preferencesReset(state) {
      Object.assign(state, createDefaultBrowserPreferences())
      state.activitySequence += 1
    },
    logicalPositionRecorded(state, action: { payload: { sessionId: string; position: string } }) {
      delete state.lastLogicalPositions[action.payload.sessionId]
      state.lastLogicalPositions[action.payload.sessionId] = action.payload.position
      const retained = Object.entries(state.lastLogicalPositions).slice(
        -MAX_SAVED_LOGICAL_POSITIONS,
      )
      state.lastLogicalPositions = Object.fromEntries(retained)
    },
    overlaySet(state, action: { payload: Overlay }) {
      state.overlay = action.payload
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
      state.originalArtifacts[action.payload] = true
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
  appSlice.actions.remoteMediaSet.type,
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
      remoteMedia: app.remoteMedia,
      lastLogicalPositions: app.lastLogicalPositions,
      keyOverrides: app.keyOverrides,
    })
  }
  return result
}

export const store = configureStore({
  reducer: { app: appSlice.reducer },
  middleware: (getDefaultMiddleware) =>
    getDefaultMiddleware().concat(traceMiddleware, preferenceMiddleware),
  devTools: { maxAge: REDUX_DEVTOOLS_ACTIONS, trace: false },
})

export const actions = appSlice.actions
export type RootState = ReturnType<typeof store.getState>
export type AppDispatch = typeof store.dispatch
export const selectApp = (state: RootState) => state.app
export const getRecentActions = (): readonly string[] => actionTrace
export const useAppDispatch = useDispatch.withTypes<AppDispatch>()
export const useAppSelector = useSelector.withTypes<RootState>()
