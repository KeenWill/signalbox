import { useForm } from '@tanstack/react-form'
import { useDebouncedValue } from '@tanstack/react-pacer'
import { Link } from '@tanstack/react-router'
import { Search } from 'lucide-react'
import { scenarios } from './platform'

function ScenarioResults({ query, activeId }: { query: string; activeId: string }) {
  const [debouncedQuery] = useDebouncedValue(query, { wait: 120 })
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
        >
          <span>{scenario.title}</span>
          <small>{scenario.description}</small>
        </Link>
      ))}
    </nav>
  )
}

export function ScenarioNavigation({ activeId }: { activeId: string }) {
  const form = useForm({ defaultValues: { query: '' } })
  return (
    <div className="scenario-navigation">
      <div className="brand">
        <span className="brand-mark">SB</span>
        <strong>Signalbox</strong>
        <small>Scenario studio</small>
      </div>
      <form className="scenario-search" onSubmit={(event) => event.preventDefault()} role="search">
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
      <form.Subscribe selector={(state) => state.values.query}>
        {(query) => <ScenarioResults query={query} activeId={activeId} />}
      </form.Subscribe>
    </div>
  )
}
