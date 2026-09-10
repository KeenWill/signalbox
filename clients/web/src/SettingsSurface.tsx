import type { ReactNode } from 'react'
import { type CommandContext, invokeCommand } from './commands'
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
  const resizePane = (
    command:
      | 'pane.navigation.preview'
      | 'pane.navigation.resize'
      | 'pane.inspector.preview'
      | 'pane.inspector.resize',
    paneSize: number,
  ) => invokeCommand(command, { ...context, paneSize })
  return (
    <div className="surface-body settings-surface">
      <div className="settings-grid">
        <PreferenceGroup legend="Layout">
          <label>
            <input
              type="radio"
              name="layout"
              checked={app.layout === 'workbench'}
              onChange={() => invokeCommand('layout.workbench', context)}
            />
            <span>Workbench</span>
          </label>
          <label>
            <input
              type="radio"
              name="layout"
              checked={app.layout === 'focus'}
              onChange={() => invokeCommand('layout.focus', context)}
            />
            <span>Focus</span>
          </label>
        </PreferenceGroup>

        <PreferenceGroup legend="Density">
          <label>
            <input
              type="radio"
              name="density"
              checked={app.density === 'compact'}
              onChange={() => invokeCommand('density.compact', context)}
            />
            <span>Compact</span>
          </label>
          <label>
            <input
              type="radio"
              name="density"
              checked={app.density === 'comfortable'}
              onChange={() => invokeCommand('density.comfortable', context)}
            />
            <span>Comfortable</span>
          </label>
        </PreferenceGroup>

        <PreferenceGroup legend="Transcript detail">
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

        <fieldset className="preference-group pane-preferences">
          <legend>Pane widths</legend>
          <label>
            <span>Navigation width</span>
            <output>{app.paneSizes.navigation}px</output>
            <input
              type="range"
              aria-label="Navigation width"
              min="160"
              max="360"
              value={app.paneSizes.navigation}
              onInput={(event) =>
                resizePane('pane.navigation.preview', event.currentTarget.valueAsNumber)
              }
              onPointerUp={(event) =>
                resizePane('pane.navigation.resize', event.currentTarget.valueAsNumber)
              }
              onKeyUp={(event) =>
                resizePane('pane.navigation.resize', event.currentTarget.valueAsNumber)
              }
              onBlur={(event) =>
                resizePane('pane.navigation.resize', event.currentTarget.valueAsNumber)
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
              onInput={(event) =>
                resizePane('pane.inspector.preview', event.currentTarget.valueAsNumber)
              }
              onPointerUp={(event) =>
                resizePane('pane.inspector.resize', event.currentTarget.valueAsNumber)
              }
              onKeyUp={(event) =>
                resizePane('pane.inspector.resize', event.currentTarget.valueAsNumber)
              }
              onBlur={(event) =>
                resizePane('pane.inspector.resize', event.currentTarget.valueAsNumber)
              }
            />
          </label>
        </fieldset>
      </div>

      <div className="settings-actions">
        <button type="button" onClick={() => invokeCommand('preferences.reset', context)}>
          Restore defaults
        </button>
      </div>
    </div>
  )
}
