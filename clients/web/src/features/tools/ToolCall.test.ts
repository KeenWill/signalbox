import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'
import { detailItems, detailPage, resultCursor } from '../../../e2e/session-detail-fixture'
import { decodeWebSessionTimelineDetailPage } from '../../generated/web-contract.mjs'
import { ToolApproval, ToolCall } from './ToolCall'
import { excerptFields, searchResultText, webLink } from './toolPresentation'
import { toolExample, toolExcerpt } from './toolScenario'

const render = (tools: ReturnType<typeof toolExample>) =>
  tools.map((tool) => renderToStaticMarkup(createElement(ToolCall, { tool }))).join('')

describe('tool presentation', () => {
  it('renders the availability of arguments and output on decoded continuation pages', () => {
    const tools = toolExample(
      'sandboxed_exec',
      { program: 'cargo', arguments: ['check'] },
      { outcome: { kind: 'exited', code: 7 } },
    )
    const item = detailItems[1]
    if (item?.body.type !== 'tool_batch') throw new Error('Tool detail fixture required')
    const body = item.body
    const pages = tools.map((tool, index) =>
      decodeWebSessionTimelineDetailPage(
        detailPage(
          [
            {
              ...item,
              projected_body_bytes:
                128 +
                Number(
                  (
                    tool.arguments ??
                    (tool.evidence.type === 'physical_attempt' ? tool.evidence.result : null)
                  )?.total_bytes,
                ),
              body: { ...body, tools: [tool] },
            },
          ],
          index === 0 ? resultCursor : null,
        ),
      ),
    )
    const markup = pages.map((page) => {
      const body = page.items[0]?.body
      if (body?.type !== 'tool_batch' || !body.tools[0]) throw new Error('Decoded tool required')
      return renderToStaticMarkup(createElement(ToolCall, { tool: body.tools[0] }))
    })
    expect(markup[0]).toContain('cargo')
    expect(markup[0]).toContain('Output is on another detail page')
    expect(markup[1]).toContain('Exit 7')
    expect(markup[1]).toContain('Arguments are on another detail page')
  })

  it.each([
    ['git_stage', { paths: ['src/main.rs'] }, 'Paths', 'src/main.rs'],
    ['git_diff', { scope: 'range', base: 'main', head: 'feature' }, 'Base', 'main'],
    ['git_log', { revision: 'feature' }, 'Revision', 'feature'],
  ] as const)('keeps selectors for %s visible', (name, args, label, value) => {
    const markup = render(toolExample(name, args, {}))
    expect(markup).toContain(`<dt>${label}</dt>`)
    expect(markup).toContain(value)
  })

  it.each([
    ['preauthorization_rejected', 'Authorization rejected'],
    ['invalid_arguments', 'Invalid arguments'],
    ['unknown_tool', 'Unknown tool'],
    ['execution_failed', 'Execution failed'],
  ] as const)('shows %s without a projected failure excerpt', (cause, label) => {
    const [tool] = toolExample('read_file', { path: 'file.txt' }, null)
    if (tool.evidence.type !== 'physical_attempt') throw new Error('Physical fixture required')
    const markup = renderToStaticMarkup(
      createElement(ToolCall, {
        tool: {
          ...tool,
          evidence: {
            ...tool.evidence,
            state: 'known_failed',
            cause,
            failure_present: true,
            failure: null,
            result_present: false,
            result: null,
          },
        },
      }),
    )
    expect(markup).toContain('Failed')
    expect(markup).toContain(label)
  })

  it.each([204, 302, 404, 503])('shows HTTP %s for an empty fetch response', (status) => {
    const markup = render(
      toolExample('web_fetch', { url: 'https://example.com/' }, { status, body: '' }),
    )
    expect(markup).toContain(`HTTP ${status}`)
    expect(markup).toContain('aria-expanded="false"')
  })

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

  it('identifies command output decoded with replacement characters', () => {
    const markup = render(
      toolExample(
        'sandboxed_exec',
        { program: 'cat', arguments: ['data.bin'] },
        { stdout: { text: 'a�b', completeness: 'complete', encoding: 'lossy_utf8' } },
      ),
    )
    expect(markup).toContain('Some output bytes could not be decoded')
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

  it('keeps custom tools with familiar names in the labeled fallback', () => {
    const markup = render(toolExample('shell', { script: 'custom syntax' }, { answer: 'done' }))
    expect(markup).toContain('<dt>Script</dt><dd>custom syntax</dd>')
  })

  it('shows file tails as partial even when the tail is complete', () => {
    expect(
      render(
        toolExample(
          'read_file',
          { path: 'file.txt', offset: 20 },
          { content: 'tail', offset: 20, truncated: false },
        ),
      ),
    ).toContain('Showing part of the file')
  })

  it('keeps collection query fields in the fallback', () => {
    const markup = render(
      toolExample(
        'search_files',
        { path: 'src', pattern: 'needle', glob: '*.rs' },
        { matches: [], truncated: true },
      ),
    )
    expect(markup).toContain('<dt>Pattern</dt><dd>needle</dd>')
    expect(markup).toContain('<dt>Glob</dt><dd>*.rs</dd>')
    expect(markup).not.toContain('Showing part of the file')
  })

  it('decodes exactly one layer of search-result escaping', () => {
    expect(searchResultText('&lt;b&gt;&quot;Rust&quot; &amp; &#x27;HTML&#x27;&lt;/b&gt;')).toBe(
      '<b>"Rust" & \'HTML\'</b>',
    )
    expect(searchResultText('&amp;lt;tag&amp;gt;')).toBe('&lt;tag&gt;')
    const markup = render(
      toolExample(
        'web_search',
        { query: 'Rust' },
        {
          results: [
            {
              url: 'https://example.com/',
              title: '&lt;b&gt;Rust&lt;/b&gt;',
              snippet: 'Rust &amp; HTML',
            },
          ],
        },
      ),
    )
    expect(markup).toContain('&lt;b&gt;Rust&lt;/b&gt;')
    expect(markup).not.toContain('<b>Rust</b>')
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

  it('links the normalized GitHub result destination', () => {
    const markup = render(
      toolExample(
        'github_pull_request_metadata',
        { repository: 'example/project', number: 12 },
        { title: 'A pull request', url: 'https://github.com/example/project/pull/12' },
      ),
    )
    expect(markup).toContain('href="https://github.com/example/project/pull/12"')
  })

  it('shows server-side truncation for Git diffs', () => {
    const markup = render(
      toolExample('git_diff', {}, { patch: 'diff --git a/file b/file', truncated: true }),
    )
    expect(markup).toContain('Showing part of the diff')
  })

  it('does not label complete non-BMP output as truncated', () => {
    const markup = render(toolExample('read_file', { path: 'emoji.txt' }, { content: '😀' }))
    expect(markup).toContain('😀')
    expect(markup).not.toContain('Showing part')
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
