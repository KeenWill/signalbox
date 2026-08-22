import type { ReactNode } from 'react'
import { defaultBrowserPreferences } from './preferences'
import { actions, selectApp, useAppDispatch, useAppSelector } from './state'

function PreferenceGroup({ legend, children }: { legend: string; children: ReactNode }) {
  return (
    <fieldset className="preference-group">
      <legend>{legend}</legend>
      <div className="preference-choices">{children}</div>
    </fieldset>
  )
}

export function SettingsSurface() {
  const app = useAppSelector(selectApp)
  const dispatch = useAppDispatch()
  return (
    <div className="surface-body settings-surface">
      <section className="settings-intro">
        <span className="eyebrow">Browser local</span>
        <h2>Operator preferences</h2>
        <p>
          Presentation choices stay in this browser. They do not change daemon authority or
          manufacture operational facts.
        </p>
      </section>

      <div className="settings-grid">
        <PreferenceGroup legend="Workspace layout">
          <label>
            <input
              type="radio"
              name="layout"
              checked={app.layout === 'workbench'}
              onChange={() => dispatch(actions.layoutSet('workbench'))}
            />
            <span>Workbench</span>
            <small>Navigation, primary surface, and contextual inspector.</small>
          </label>
          <label>
            <input
              type="radio"
              name="layout"
              checked={app.layout === 'focus'}
              onChange={() => dispatch(actions.layoutSet('focus'))}
            />
            <span>Focus</span>
            <small>A quiet primary surface with secondary panes removed.</small>
          </label>
        </PreferenceGroup>

        <PreferenceGroup legend="Visual density">
          <label>
            <input
              type="radio"
              name="density"
              checked={app.density === 'compact'}
              onChange={() => dispatch(actions.densitySet('compact'))}
            />
            <span>Compact</span>
            <small>Dense rows for high-volume operator work.</small>
          </label>
          <label>
            <input
              type="radio"
              name="density"
              checked={app.density === 'comfortable'}
              onChange={() => dispatch(actions.densitySet('comfortable'))}
            />
            <span>Comfortable</span>
            <small>More separation without changing information detail.</small>
          </label>
        </PreferenceGroup>

        <PreferenceGroup legend="Transcript presentation">
          <label>
            <input
              type="radio"
              name="detail"
              checked={app.detail === 'full'}
              onChange={() => dispatch(actions.detailSet('full'))}
            />
            <span>Full</span>
          </label>
          <label>
            <input
              type="radio"
              name="detail"
              checked={app.detail === 'condensed'}
              onChange={() => dispatch(actions.detailSet('condensed'))}
            />
            <span>Condensed</span>
          </label>
          <label>
            <input
              type="radio"
              name="detail"
              checked={app.detail === 'results'}
              onChange={() => dispatch(actions.detailSet('results'))}
            />
            <span>Results</span>
          </label>
        </PreferenceGroup>

        <PreferenceGroup legend="Theme">
          <label>
            <input
              type="radio"
              name="theme"
              checked={app.theme === 'dark'}
              onChange={() => dispatch(actions.themeSet('dark'))}
            />
            <span>Dark</span>
          </label>
          <label>
            <input
              type="radio"
              name="theme"
              checked={app.theme === 'light'}
              onChange={() => dispatch(actions.themeSet('light'))}
            />
            <span>Light</span>
          </label>
        </PreferenceGroup>

        <PreferenceGroup legend="Remote media">
          <label>
            <input
              type="radio"
              name="remote-media"
              checked={app.remoteMedia === 'ask'}
              onChange={() => dispatch(actions.remoteMediaSet('ask'))}
            />
            <span>Ask before loading</span>
          </label>
          <label>
            <input
              type="radio"
              name="remote-media"
              checked={app.remoteMedia === 'block'}
              onChange={() => dispatch(actions.remoteMediaSet('block'))}
            />
            <span>Block</span>
          </label>
          <label>
            <input
              type="radio"
              name="remote-media"
              checked={app.remoteMedia === 'allow'}
              onChange={() => dispatch(actions.remoteMediaSet('allow'))}
            />
            <span>Allow</span>
          </label>
        </PreferenceGroup>

        <fieldset className="preference-group pane-preferences">
          <legend>Workbench panes</legend>
          <label>
            <span>Navigation width</span>
            <output>{app.paneSizes.navigation}px</output>
            <input
              type="range"
              aria-label="Navigation width"
              min="160"
              max="360"
              value={app.paneSizes.navigation}
              onChange={(event) =>
                dispatch(
                  actions.paneSizesSet({
                    ...app.paneSizes,
                    navigation: event.currentTarget.valueAsNumber,
                  }),
                )
              }
            />
          </label>
          <label>
            <span>Inspector width</span>
            <output>{app.paneSizes.inspector}px</output>
            <input
              type="range"
              aria-label="Inspector width"
              min="200"
              max="480"
              value={app.paneSizes.inspector}
              onChange={(event) =>
                dispatch(
                  actions.paneSizesSet({
                    ...app.paneSizes,
                    inspector: event.currentTarget.valueAsNumber,
                  }),
                )
              }
            />
          </label>
        </fieldset>
      </div>

      <div className="settings-actions">
        <button type="button" onClick={() => dispatch(actions.preferencesReset())}>
          Restore defaults
        </button>
        <small>
          Defaults: {defaultBrowserPreferences.layout}, {defaultBrowserPreferences.density},{' '}
          {defaultBrowserPreferences.detail}, {defaultBrowserPreferences.theme}.
        </small>
      </div>
    </div>
  )
}
