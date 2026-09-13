import { createRoot } from 'react-dom/client'
import '../../app.css'
import { ToolCall } from './ToolCall'
import { jsonExamples, toolExamples } from './toolScenario'

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
      {jsonExamples.map(({ name, tool }) => (
        <ToolCall key={name} tool={tool} />
      ))}
      {toolExamples.flat().map((tool) => (
        <ToolCall
          key={`${tool.tool_name}:${tool.arguments ? 'arguments' : 'output'}`}
          tool={tool}
        />
      ))}
    </main>,
  )
