import { useForm } from '@tanstack/react-form'
import { useDebouncedValue } from '@tanstack/react-pacer'
import { Link } from '@tanstack/react-router'
import { Search } from 'lucide-react'
import { scenarios } from './platform'

// Tunable effective ceiling: batch keystrokes to bound repeated catalog filtering
// while keeping the scenario results perceptually responsive.
const SCENARIO_SEARCH_DEBOUNCE_MS = 120

function ScenarioResults({
  query,
  activeId,
  onSelect,
  disabled = false,
}: {
  query: string
  activeId: string
  onSelect?: () => void
  disabled?: boolean
}) {
  const [debouncedQuery] = useDebouncedValue(query, { wait: SCENARIO_SEARCH_DEBOUNCE_MS })
  const normalized = debouncedQuery.trim().toLowerCase()
  const visible = scenarios.filter((scenario) =>
    `${scenario.title} ${scenario.description}`.toLowerCase().includes(normalized),
  )
  return (
    <nav aria-label="Development scenarios" className="scenario-list">
      {visible.map((scenario) => (
        <Link
          key={scenario.id}
          to="/scenario/$scenarioId"
          params={{ scenarioId: scenario.id }}
          className={activeId === scenario.id ? 'scenario-link active' : 'scenario-link'}
          aria-current={activeId === scenario.id ? 'page' : undefined}
          aria-disabled={disabled || undefined}
          tabIndex={disabled ? -1 : undefined}
          onClick={(event) => {
            if (disabled) event.preventDefault()
            else onSelect?.()
          }}
        >
          <span>{scenario.title}</span>
          <small>{scenario.description}</small>
        </Link>
      ))}
    </nav>
  )
}

export function ScenarioNavigation({
  activeId,
  onSelect,
  disabled = false,
}: {
  activeId: string
  onSelect?: () => void
  disabled?: boolean
}) {
  const form = useForm({ defaultValues: { query: '' } })
  return (
    <div className="scenario-navigation">
      <div className="brand">
        <span className="brand-mark">SB</span>
        <strong>Signalbox</strong>
        <small>Scenario studio</small>
      </div>
      <search>
        <form className="scenario-search" onSubmit={(event) => event.preventDefault()}>
          <Search aria-hidden="true" />
          <form.Field name="query">
            {(field) => (
              <input
                aria-label="Filter scenarios"
                placeholder="Filter scenarios"
                value={field.state.value}
                onChange={(event) => field.handleChange(event.target.value)}
              />
            )}
          </form.Field>
        </form>
      </search>
      <form.Subscribe selector={(state) => state.values.query}>
        {(query) => (
          <ScenarioResults
            query={query}
            activeId={activeId}
            onSelect={onSelect}
            disabled={disabled}
          />
        )}
      </form.Subscribe>
    </div>
  )
}
