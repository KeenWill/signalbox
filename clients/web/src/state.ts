import { configureStore, createSlice, type Middleware } from '@reduxjs/toolkit'
import { useDispatch, useSelector } from 'react-redux'

export type LayoutMode = 'focus' | 'workbench'
export type DensityMode = 'compact' | 'comfortable'
export type DetailMode = 'full' | 'condensed' | 'results'
export type ThemeMode = 'light' | 'dark'
export type Overlay = 'palette' | 'help' | 'navigation' | null

interface AppState {
  layout: LayoutMode
  density: DensityMode
  detail: DetailMode
  theme: ThemeMode
  overlay: Overlay
  selectedTimeline: number
  transcriptRange: [number, number]
  tableRange: [number, number]
  activitySequence: number
}

const initialState: AppState = {
  layout: 'workbench',
  density: 'compact',
  detail: 'condensed',
  theme: 'dark',
  overlay: null,
  selectedTimeline: 0,
  transcriptRange: [0, 0],
  tableRange: [0, 0],
  activitySequence: 0,
}

// Hard safety ceiling: diagnostics retain only enough Redux activity for local triage.
const RECENT_REDUX_ACTIONS = 18
// Hard safety ceiling: Redux DevTools cannot retain an unbounded interaction history.
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
    overlaySet(state, action: { payload: Overlay }) {
      state.overlay = action.payload
      state.activitySequence += 1
    },
    timelineSelected(state, action: { payload: number }) {
      state.selectedTimeline = Math.max(0, action.payload)
      state.activitySequence += 1
    },
    transcriptRangeSet(state, action: { payload: [number, number] }) {
      state.transcriptRange = action.payload
    },
    tableRangeSet(state, action: { payload: [number, number] }) {
      state.tableRange = action.payload
    },
  },
})

const actionTrace: string[] = []
const traceMiddleware: Middleware = () => (next) => (action) => {
  if (typeof action === 'object' && action !== null && 'type' in action) {
    actionTrace.push(String(action.type))
    if (actionTrace.length > RECENT_REDUX_ACTIONS) actionTrace.shift()
  }
  return next(action)
}

export const store = configureStore({
  reducer: { app: appSlice.reducer },
  middleware: (getDefaultMiddleware) => getDefaultMiddleware().concat(traceMiddleware),
  devTools: { maxAge: REDUX_DEVTOOLS_ACTIONS, trace: false },
})

export const actions = appSlice.actions
export type RootState = ReturnType<typeof store.getState>
export type AppDispatch = typeof store.dispatch
export const selectApp = (state: RootState) => state.app
export const getRecentActions = (): readonly string[] => actionTrace
export const useAppDispatch = useDispatch.withTypes<AppDispatch>()
export const useAppSelector = useSelector.withTypes<RootState>()
