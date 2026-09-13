import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'
import { detailItems, detailPage, resultCursor } from '../../../e2e/session-detail-fixture'
import { decodeWebSessionTimelineDetailPage } from '../../generated/web-contract.mjs'
import { ToolApproval, ToolCall } from './ToolCall'
import { excerptFields, previewText, searchResultText, webLink } from './toolPresentation'
import { toolExample, toolExcerpt } from './toolScenario'

const render = (tools: ReturnType<typeof toolExample>) =>
  tools.map((tool) => renderToStaticMarkup(createElement(ToolCall, { tool }))).join('')

describe('tool presentation', () => {
  it.each([0, 128])('shows paged file_read output at byte %s', (offset) => {
    const [, tool] = toolExample('file_read', { view: 'text' }, {})
    if (tool.evidence.type !== 'physical_attempt') throw new Error('Physical fixture required')
    const text = 'remaining file contents'
    const result = {
      ...toolExcerpt(text),
      offset_bytes: String(offset),
      total_bytes: String(offset + text.length + 10),
      continuation: { ...resultCursor.body, offset_bytes: String(offset + text.length) },
    }
    const markup = renderToStaticMarkup(
      createElement(ToolCall, { tool: { ...tool, evidence: { ...tool.evidence, result } } }),
    )
    expect(markup).toContain(text)
    expect(markup).toContain('Showing part of output')
  })

  it('summarizes media reads without displaying their digest', () => {
    const markup = render(
      toolExample(
        'file_read',
        { digest: 'sha256:source', view: 'page_image', options: { page: 2 } },
        {
          status: 'read',
          output: 'image',
          digest: 'sha256:result',
          media_type: 'image/png',
          byte_length: '100',
        },
      ),
    )
    expect(markup).toContain('Page image')
    expect(markup).toContain('Image returned')
    expect(markup).not.toContain('sha256:')
  })

  it.each([0, 512])('retains the requested read window at byte %s', (offset) => {
    const [tool] = toolExample('read_file', { path: 'file.txt', offset, max_bytes: 1024 }, {})
    const markup = renderToStaticMarkup(createElement(ToolCall, { tool }))
    expect(markup).toContain(`Starting byte: ${offset}`)
    expect(markup).toContain('Maximum bytes: 1024')
  })

  it.each(['one\r\ntwo\r\n', '10%\r20%\r', '\r\n😀\r'])(
    'preserves evidence separators in %j',
    (text) => {
      expect(previewText(text)).toEqual({ content: text, omittedCharacters: 0 })
    },
  )

  it('bounds CRLF lines without changing retained separators', () => {
    const line = 'line\r\n'
    const text = line.repeat(40)
    const preview = previewText(text)
    expect(preview.content).toBe(line.repeat(31) + 'line')
    expect(preview.omittedCharacters).toBe(text.length - preview.content.length)
  })

  it('bounds Unicode characters without splitting surrogate pairs', () => {
    expect(previewText('😀'.repeat(4001))).toEqual({
      content: '😀'.repeat(4000),
      omittedCharacters: 1,
    })
  })

  it('labels an empty search response on its result page', () => {
    const [, tool] = toolExample(
      'web_search',
      { query: 'absent' },
      { results: [], truncated: false },
    )
    expect(renderToStaticMarkup(createElement(ToolCall, { tool }))).toContain('No results returned')
  })

  it.each(['application/json', 'text/html; charset=utf-8', 'application/octet-stream'])(
    'retains response content type %s',
    (contentType) => {
      const [, tool] = toolExample(
        'web_fetch',
        { url: 'https://example.com/' },
        {
          status: 200,
          content_type: contentType,
          body: 'response',
        },
      )
      expect(renderToStaticMarkup(createElement(ToolCall, { tool }))).toContain(
        `Content type: ${contentType}`,
      )
    },
  )

  it.each(['9007199254740993', '-9007199254740993', '1e400', '-0', '1.00', '1e3'])(
    'preserves the original JSON number %s',
    (number) => {
      const [argumentsTool, resultTool] = toolExample('custom_tool', {}, {})
      const text = `{"nested":{"nonce":${number}}}`
      if (resultTool.evidence.type !== 'physical_attempt')
        throw new Error('Physical fixture required')
      for (const tool of [
        { ...argumentsTool, arguments: toolExcerpt(text) },
        { ...resultTool, evidence: { ...resultTool.evidence, result: toolExcerpt(text) } },
      ]) {
        const markup = renderToStaticMarkup(createElement(ToolCall, { tool }))
        expect(markup).toContain(number)
        expect(markup).not.toContain('9007199254740992')
      }
      expect(excerptFields(toolExcerpt(text))).toEqual({})
    },
  )

  it('still parses safe integers for typed summaries', () => {
    expect(excerptFields(toolExcerpt('{"nonce":9007199254740991}'))).toEqual({
      nonce: Number.MAX_SAFE_INTEGER,
    })
  })

  it('retains proposed fields before a familiar tool has validation evidence', () => {
    const [tool] = toolExample('read_file', { resource: 'x' }, {})
    const markup = renderToStaticMarkup(
      createElement(ToolCall, {
        tool: {
          ...tool,
          evidence: { type: 'request_only' },
        },
      }),
    )
    expect(markup).toContain('<dt>Resource</dt><dd>x</dd>')
    expect(markup).toContain('Requested')
  })

  it('summarizes a complete large file result without dropping its partial-file evidence', () => {
    const markup = render(
      toolExample(
        'read_file',
        { path: 'file.txt' },
        {
          content: 'x'.repeat(20 * 1024),
          truncated: true,
          next_offset: 20 * 1024,
        },
      ),
    )
    expect(markup).toContain('aria-label="File contents"')
    expect(markup).toContain('Showing part of the file')
    expect(markup.length).toBeLessThan(6_000)
  })

  it('retains the outcome after large complete command output', () => {
    const markup = render(
      toolExample(
        'sandboxed_exec',
        { program: 'tool' },
        {
          stdout: { text: 'x'.repeat(20 * 1024) },
          outcome: { kind: 'exited', code: 7 },
        },
      ),
    )
    expect(markup).toContain('Exit 7')
    expect(markup.length).toBeLessThan(6_000)
  })

  it('retains links in complete large search results', () => {
    const markup = render(
      toolExample(
        'web_search',
        { query: 'example' },
        {
          results: [
            { snippet: 'x'.repeat(20 * 1024), title: 'Example', url: 'https://example.com/' },
          ],
        },
      ),
    )
    expect(markup).toContain('href="https://example.com/"')
    expect(markup.length).toBeLessThan(6_000)
  })

  it('bounds structured parsing by UTF-8 bytes instead of display characters', () => {
    // The generated timeline-detail body contract admits at most 64 KiB.
    const limit = 64 * 1024
    const envelope = JSON.stringify({ value: '' }).length
    const text = JSON.stringify({ value: 'x'.repeat(limit - envelope) })
    expect(excerptFields(toolExcerpt(text))).toHaveProperty('value')
    expect(excerptFields(toolExcerpt(`${text} `))).toEqual({})
    expect(excerptFields(toolExcerpt(JSON.stringify({ value: '😀'.repeat(limit / 4) })))).toEqual(
      {},
    )
  })

  it('labels a successful empty file read on the result page', () => {
    const [, tool] = toolExample(
      'read_file',
      { path: 'empty.txt' },
      { content: '', offset: 0, truncated: false },
    )
    const markup = renderToStaticMarkup(createElement(ToolCall, { tool }))
    expect(markup).toContain('No content in this read')
    expect(markup).not.toContain('Showing part of the file')
  })

  it('retains generic arguments when a familiar tool name is unknown to the catalog', () => {
    const [tool] = toolExample('read_file', { resource: 'x' }, {})
    if (tool.evidence.type !== 'physical_attempt') throw new Error('Physical fixture required')
    const markup = renderToStaticMarkup(
      createElement(ToolCall, {
        tool: {
          ...tool,
          evidence: {
            ...tool.evidence,
            state: 'known_failed',
            cause: 'unknown_tool',
            result_present: false,
            failure_present: true,
          },
        },
      }),
    )
    expect(markup).toContain('<dt>Resource</dt><dd>x</dd>')
    expect(markup).toContain('Unknown tool')
  })

  it.each(['missing', 'unusable'])('identifies %s sandbox availability', (availability) => {
    const [, tool] = toolExample(
      'sandboxed_exec',
      { program: 'tool' },
      {
        confinement: { kind: 'sandbox_refused', availability },
        outcome: { kind: 'spawn_failed', reason: 'sandbox_unavailable' },
      },
    )
    const markup = renderToStaticMarkup(createElement(ToolCall, { tool }))
    expect(markup).toContain(
      `Sandbox availability: ${availability === 'missing' ? 'Missing' : 'Unusable'}`,
    )
  })

  it.each(['network_fence_active', null])('preserves the timeout diagnostic %s', (diagnostic) => {
    const [, tool] = toolExample(
      'sandboxed_exec',
      { program: 'tool' },
      {
        confinement: { kind: 'filesystem_confined' },
        outcome: { kind: 'timed_out' },
        diagnostic,
      },
    )
    const markup = renderToStaticMarkup(createElement(ToolCall, { tool }))
    expect(markup).toContain('Timed out')
    expect(markup.includes('Network fence active')).toBe(diagnostic !== null)
  })

  it('retains rejected argument values in the labeled fallback', () => {
    const [tool] = toolExample('sandboxed_exec', { program: 'cargo', arguments: 42 }, {})
    if (tool.evidence.type !== 'physical_attempt') throw new Error('Physical fixture required')
    const markup = renderToStaticMarkup(
      createElement(ToolCall, {
        tool: {
          ...tool,
          evidence: {
            ...tool.evidence,
            state: 'known_failed',
            cause: 'invalid_arguments',
            result_present: false,
            failure_present: true,
          },
        },
      }),
    )
    expect(markup).toContain('<dt>Arguments</dt><dd>42</dd>')
    expect(markup).toContain('<dt>Program</dt><dd>cargo</dd>')
    expect(markup).toContain('Invalid arguments')
  })

  it.each(['sandboxed_exec', 'unsandboxed_exec'])('shows the explicit timeout for %s', (name) => {
    const [tool] = toolExample(name, { program: 'tool', timeout_seconds: 17 }, {})
    expect(renderToStaticMarkup(createElement(ToolCall, { tool }))).toContain('Timeout: 17 seconds')
  })

  it.each([
    ['user', 'User'],
    ['user_override', 'User override'],
  ] as const)('labels the %s approval actor without viewer attribution', (type, label) => {
    const markup = renderToStaticMarkup(
      createElement(ToolApproval, {
        approval: {
          type: 'tool_approval_decision',
          tool_name: 'unsandboxed_exec',
          decision: 'approve',
          actor:
            type === 'user_override'
              ? {
                  type,
                  command_id: '00000000-0000-7000-8000-000000000003',
                  denied_request_id: '00000000-0000-7000-8000-000000000004',
                }
              : { type, command_id: '00000000-0000-7000-8000-000000000003' },
          request_id: '00000000-0000-7000-8000-000000000001',
          turn_id: '00000000-0000-7000-8000-000000000002',
          approval_judge_escalated: false,
        },
      }),
    )
    expect(markup).toContain(`<span>${label}</span>`)
    expect(markup).not.toContain('You')
  })

  it.each([
    ['sandbox_setup_failed', 'Sandbox setup failed'],
    ['filesystem_confined', 'Filesystem confined'],
    ['unsandboxed', 'Unsandboxed'],
  ])('identifies the execution boundary for a %s timeout', (kind, label) => {
    const [, tool] = toolExample(
      'sandboxed_exec',
      { program: 'tool' },
      {
        confinement: { kind },
        outcome: { kind: 'timed_out' },
      },
    )
    const markup = renderToStaticMarkup(createElement(ToolCall, { tool }))
    expect(markup).toContain('Timed out')
    expect(markup).toContain(`<span>${label}</span>`)
    expect(markup.includes('Sandbox setup failed')).toBe(kind === 'sandbox_setup_failed')
  })

  it.each([null, 0, 7])('distinguishes a signal from exit code %s', (code) => {
    const markup = render(
      toolExample('sandboxed_exec', { program: 'tool' }, { outcome: { kind: 'exited', code } }),
    )
    expect(markup.includes('Terminated by signal')).toBe(code === null)
    if (code !== null) expect(markup).toContain(`Exit ${code}`)
    else expect(markup).not.toContain('<span>Exited</span>')
  })

  it.each(['', 'contents', undefined])(
    'identifies explicitly empty write content %s',
    (content) => {
      const [tool] = toolExample('write_file', { path: 'file.txt', content }, {})
      const markup = renderToStaticMarkup(createElement(ToolCall, { tool }))
      expect(markup.includes('Write empty file')).toBe(content === '')
      expect(markup).toContain('file.txt')
    },
  )

  it.each([
    ['spawn_failed', 'not_found', 'Not found'],
    ['spawn_failed', 'permission_denied', 'Permission denied'],
    ['supervision_failed', 'stdout', 'Stdout'],
    ['supervision_failed', 'cleanup', 'Cleanup'],
  ])('shows the %s reason %s', (kind, reason, label) => {
    const markup = render(
      toolExample('sandboxed_exec', { program: 'tool' }, { outcome: { kind, reason } }),
    )
    expect(markup).toContain(`<span>${label}</span>`)
  })

  it('preserves the executable as one quoted argv element', () => {
    const [tool] = toolExample(
      'unsandboxed_exec',
      { program: 'tools/my "command"', arguments: ['a b'] },
      {},
    )
    const markup = renderToStaticMarkup(createElement(ToolCall, { tool }))
    const expected = renderToStaticMarkup(
      createElement('code', null, '"tools/my \\"command\\"" "a b"'),
    )
    expect(markup).toContain(expected)
  })

  it.each([true, false, undefined])(
    'identifies replace-all edits when replace_all is %s',
    (replaceAll) => {
      const [tool] = toolExample(
        'edit_file',
        { path: 'file.txt', old_string: 'before', new_string: 'after', replace_all: replaceAll },
        { replacements: 3 },
      )
      const markup = renderToStaticMarkup(createElement(ToolCall, { tool }))
      expect(markup.includes('Replace every match')).toBe(replaceAll === true)
      expect(markup).toContain('Output is on another detail page')
    },
  )

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
