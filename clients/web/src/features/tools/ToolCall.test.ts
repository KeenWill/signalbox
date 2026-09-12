import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'
import { ToolApproval, ToolCall } from './ToolCall'
import { excerptFields, webLink } from './toolPresentation'
import { toolExample, toolExcerpt } from './toolScenario'

const render = (tool: ReturnType<typeof toolExample>) =>
  renderToStaticMarkup(createElement(ToolCall, { tool }))

describe('tool presentation', () => {
  it('shows direct execution output and its exit code', () => {
    const markup = render(
      toolExample(
        'sandboxed_exec',
        { program: 'cargo', arguments: ['check'] },
        { outcome: { kind: 'exited', code: 7 }, stdout: { text: 'Build failed' } },
      ),
    )
    expect(markup).toContain('cargo')
    expect(markup).toContain('Exit 7')
    expect(markup).toContain('Build failed')
    expect(markup).not.toContain('&quot;stdout&quot;')
  })

  it('renders file replacements with textual diff markers', () => {
    const markup = render(
      toolExample(
        'edit_file',
        { path: 'main.rs', old_string: 'before', new_string: 'after' },
        { replacements: 1 },
      ),
    )
    expect(markup).toContain('main.rs')
    expect(markup).toContain('- before')
    expect(markup).toContain('+ after')
  })

  it('shows unknown tool values with readable field labels', () => {
    expect(
      render(toolExample('custom_tool', { input_path: 'sample.txt' }, { answer: 42 })),
    ).toContain('<dt>Input path</dt><dd>sample.txt</dd>')
  })

  it('does not interpret partial JSON as complete tool arguments', () => {
    expect(excerptFields({ ...toolExcerpt('{"path":"tail"}'), offset_bytes: '123' })).toEqual({})
  })

  it('keeps a huge tool result bounded in the DOM', () => {
    const markup = render(
      toolExample('read_file', { path: 'large.txt' }, { content: 'x'.repeat(1_000_000) }),
    )
    expect(markup.length).toBeLessThan(6_000)
    expect(markup).toContain('Showing part of the text')
  })

  it('does not turn executable URLs into links', () => {
    expect(webLink('javascript:alert(1)')).toBeUndefined()
  })

  it('shows the recorded approval actor and reason', () => {
    const markup = renderToStaticMarkup(
      createElement(ToolApproval, {
        approval: {
          type: 'tool_approval_decision',
          tool_name: 'unsandboxed_exec',
          decision: 'deny',
          actor: { type: 'policy' },
          rationale: toolExcerpt('Command is outside the workspace'),
          request_id: '00000000-0000-7000-8000-000000000001',
          turn_id: '00000000-0000-7000-8000-000000000002',
          approval_judge_escalated: false,
        },
      }),
    )
    expect(markup).toContain('Denied')
    expect(markup).toContain('Policy')
    expect(markup).toContain('Command is outside the workspace')
  })
})
