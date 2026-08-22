import type { ReactNode } from 'react'
import { type CommandContext, invokeCommand } from './commands'
import { defaultBrowserPreferences } from './preferences'
import { selectApp, useAppSelector } from './state'

function PreferenceGroup({ legend, children }: { legend: string; children: ReactNode }) {
  return (
    <fieldset className="preference-group">
      <legend>{legend}</legend>
      <div className="preference-choices">{children}</div>
    </fieldset>
  )
}

export function SettingsSurface({ context }: { context: CommandContext }) {
  const app = useAppSelector(selectApp)
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
              onChange={() => invokeCommand('layout.workbench', context)}
            />
            <span>Workbench</span>
            <small>Navigation, primary surface, and contextual inspector.</small>
          </label>
          <label>
            <input
              type="radio"
              name="layout"
              checked={app.layout === 'focus'}
              onChange={() => invokeCommand('layout.focus', context)}
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
              onChange={() => invokeCommand('density.compact', context)}
            />
            <span>Compact</span>
            <small>Dense rows for high-volume operator work.</small>
          </label>
          <label>
            <input
              type="radio"
              name="density"
              checked={app.density === 'comfortable'}
              onChange={() => invokeCommand('density.comfortable', context)}
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
              onChange={() => invokeCommand('detail.full', context)}
            />
            <span>Full</span>
          </label>
          <label>
            <input
              type="radio"
              name="detail"
              checked={app.detail === 'condensed'}
              onChange={() => invokeCommand('detail.condensed', context)}
            />
            <span>Condensed</span>
          </label>
          <label>
            <input
              type="radio"
              name="detail"
              checked={app.detail === 'results'}
              onChange={() => invokeCommand('detail.results', context)}
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
              onChange={() => invokeCommand('theme.dark', context)}
            />
            <span>Dark</span>
          </label>
          <label>
            <input
              type="radio"
              name="theme"
              checked={app.theme === 'light'}
              onChange={() => invokeCommand('theme.light', context)}
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
              onChange={() => invokeCommand('remote-media.ask', context)}
            />
            <span>Ask before loading</span>
          </label>
          <label>
            <input
              type="radio"
              name="remote-media"
              checked={app.remoteMedia === 'block'}
              onChange={() => invokeCommand('remote-media.block', context)}
            />
            <span>Block</span>
          </label>
          <label>
            <input
              type="radio"
              name="remote-media"
              checked={app.remoteMedia === 'allow'}
              onChange={() => invokeCommand('remote-media.allow', context)}
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
              min="160"
              max="360"
              value={app.paneSizes.navigation}
              onChange={(event) =>
                invokeCommand('pane.navigation.resize', {
                  ...context,
                  paneSize: event.currentTarget.valueAsNumber,
                })
              }
            />
          </label>
          <label>
            <span>Inspector width</span>
            <output>{app.paneSizes.inspector}px</output>
            <input
              type="range"
              min="200"
              max="480"
              value={app.paneSizes.inspector}
              onChange={(event) =>
                invokeCommand('pane.inspector.resize', {
                  ...context,
                  paneSize: event.currentTarget.valueAsNumber,
                })
              }
            />
          </label>
        </fieldset>
      </div>

      <div className="settings-actions">
        <button type="button" onClick={() => invokeCommand('preferences.reset', context)}>
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
