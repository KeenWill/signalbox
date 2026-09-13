import { createRoot } from 'react-dom/client'
import '../../app.css'
import { ToolApproval, ToolCall } from './ToolCall'
import {
  argumentExamples,
  emptyStructuredRead,
  jsonExamples,
  partialApproval,
  scalarStructuredRead,
  toolExamples,
} from './toolScenario'

const root = document.getElementById('root')
for (const element of [document.documentElement, document.body, root]) {
  if (element) {
    element.style.height = 'auto'
    element.style.overflow = 'visible'
  }
}
if (root)
  createRoot(root).render(
    <main style={{ maxWidth: '60rem', margin: '1rem auto', padding: '1rem' }}>
      <h1>Tool calls</h1>
      {location.search.includes('empty-read') && (
        <section aria-label="Empty file read scenario">
          <ToolCall tool={emptyStructuredRead} />
        </section>
      )}
      <section aria-label="Scalar file read scenario">
        <ToolCall tool={scalarStructuredRead} />
      </section>
      <ToolApproval approval={partialApproval} />
      {argumentExamples.map(({ name, tool }) => (
        <ToolCall key={name} tool={tool} />
      ))}
      {jsonExamples.map(({ name, tool }) => (
        <section key={name} aria-label={`Tool scenario ${name}`}>
          <ToolCall tool={tool} />
        </section>
      ))}
      {toolExamples.flat().map((tool) => (
        <ToolCall
          key={`${tool.tool_name}:${tool.arguments ? 'arguments' : 'output'}`}
          tool={tool}
        />
      ))}
    </main>,
  )
