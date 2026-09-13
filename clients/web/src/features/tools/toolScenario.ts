import type {
  WebTimelineTextExcerpt,
  WebTimelineToolAttempt,
} from '../../generated/web-contract.mjs'

export const toolExcerpt = (text: string): WebTimelineTextExcerpt => ({
  text,
  offset_bytes: '0',
  total_bytes: String(new TextEncoder().encode(text).length),
  continuation: null,
})

// Synthetic identities and payloads for isolated renderer evidence.
export const toolExample = (
  name: string,
  args: unknown,
  result: unknown,
): [WebTimelineToolAttempt, WebTimelineToolAttempt] => {
  const tool: WebTimelineToolAttempt = {
    request_id: '00000000-0000-7000-8000-000000000001',
    tool_name: name,
    approval_posture: 'auto',
    approval_judge_escalated: false,
    arguments: toolExcerpt(JSON.stringify(args)),
    evidence: {
      type: 'physical_attempt',
      attempt_id: '00000000-0000-7000-8000-000000000002',
      state: 'completed',
      effect_posture: 'effect_free',
      result_present: true,
      failure_present: false,
      result: null,
    },
  }
  if (tool.evidence.type !== 'physical_attempt') throw new Error('Physical fixture required')
  return [
    tool,
    {
      ...tool,
      arguments: null,
      evidence: { ...tool.evidence, result: toolExcerpt(JSON.stringify(result)) },
    },
  ]
}

export const fileEvidence = 'fn main() {\r\n    println!("Hello");\r\n}\r'
export const rawFileEvidence = JSON.stringify({ content: fileEvidence }, null, 2).replaceAll(
  '\n',
  '\r\n',
)
const fileExample = toolExample('read_file', { path: 'src/main.rs' }, { content: fileEvidence })
if (fileExample[1].evidence?.type === 'physical_attempt')
  fileExample[1] = {
    ...fileExample[1],
    evidence: { ...fileExample[1].evidence, result: toolExcerpt(rawFileEvidence) },
  }

export const longEvidence = 'Fetched line\r\n'.repeat(400) + 'Last fetched line'
export const toolExamples = [
  toolExample('long_evidence', { requested: true }, { content: longEvidence }),
  toolExample('git_diff', { scope: 'working_tree' }, { patch: '-before\n+after\n' }),
  toolExample(
    'sandboxed_exec',
    { program: 'cargo', arguments: ['check'] },
    {
      outcome: { kind: 'exited', code: 0 },
      stdout: { text: 'Finished successfully', completeness: 'complete' },
    },
  ),
  fileExample,
  toolExample(
    'edit_file',
    { path: 'src/main.rs', old_string: 'Hello', new_string: 'Welcome' },
    { replacements: 1 },
  ),
  toolExample(
    'web_search',
    { query: 'Rust documentation' },
    {
      results: [
        {
          url: 'https://doc.rust-lang.org/',
          title: 'Rust documentation',
          snippet: 'The Rust programming language.',
        },
      ],
    },
  ),
  toolExample('git_branch_switch', { name: 'feature/renderers' }, { branch: 'feature/renderers' }),
  toolExample(
    'github_pull_request_metadata',
    { repository: 'example/project', number: 12 },
    { title: 'Render tool calls', url: 'https://github.com/example/project/pull/12' },
  ),
  toolExample('custom_tool', { greeting: 'Hello' }, { answer: 'World' }),
]

const jsonCase = (
  name: string,
  args: unknown,
  result: unknown,
  label: 'Arguments' | 'Output' = 'Output',
) => {
  const [request, output] = toolExample(name, args, result)
  return {
    name,
    tool: { ...output, arguments: request.arguments },
    label,
    raw: JSON.stringify(label === 'Arguments' ? args : result),
  }
}
export const jsonExamples = [
  jsonCase(
    'json_multiline_fields',
    {},
    Object.fromEntries(
      Array.from({ length: 32 }, (_, index) => [`field_${index}`, 'a\nb\rc\r\nd']),
    ),
  ),
  jsonCase(
    'json_oversized',
    { description: 'detail '.repeat(1000), last_field: 'end' },
    {},
    'Arguments',
  ),
  jsonCase(
    'json_many_fields',
    {},
    Object.fromEntries(Array.from({ length: 80 }, (_, index) => [`field_${index}`, index])),
  ),
  jsonCase('json_nested', {}, { nested: { subject: 'nested evidence' }, rows: [{ value: 1 }] }),
  jsonCase('json_fallback', {}, ['first', { second: true }]),
  jsonCase('json_empty', {}, {}),
  jsonCase('json_number', {}, { nonce: 9007199254740991 }),
  jsonCase('json_failure', {}, {}),
  jsonCase('json_partial_arguments', {}, {}, 'Arguments'),
  jsonCase('file_read', { view: 'text' }, {}),
]
const failureCase = jsonExamples.find((entry) => entry.name === 'json_failure')
if (failureCase?.tool.evidence.type === 'physical_attempt') {
  const failure = JSON.stringify({
    message: 'Cannot read attachment',
    detail: { reason: 'missing' },
  })
  failureCase.tool = {
    ...failureCase.tool,
    evidence: {
      ...failureCase.tool.evidence,
      state: 'known_failed',
      failure_present: true,
      failure: toolExcerpt(failure),
    },
  }
  failureCase.raw = failure
}
for (const entry of jsonExamples) {
  if (entry.name === 'json_number' && entry.tool.evidence.type === 'physical_attempt') {
    entry.raw = '{"nonce":9007199254740993}'
    entry.tool = {
      ...entry.tool,
      evidence: { ...entry.tool.evidence, result: toolExcerpt(entry.raw) },
    }
  }
  if (entry.name === 'json_partial_arguments') {
    entry.raw = '{"query":"partial'
    entry.tool = {
      ...entry.tool,
      arguments: {
        ...toolExcerpt(entry.raw),
        offset_bytes: '10',
        total_bytes: String(entry.raw.length + 10),
      },
    }
  }
  if (entry.name === 'file_read' && entry.tool.evidence.type === 'physical_attempt') {
    entry.raw = 'remaining body","truncated":true}'
    entry.tool = {
      ...entry.tool,
      evidence: {
        ...entry.tool.evidence,
        result: {
          ...toolExcerpt(entry.raw),
          offset_bytes: '128',
          total_bytes: String(entry.raw.length + 128),
        },
      },
    }
  }
}

export const argumentExamples = [
  { name: 'absent_arguments', arguments: null, empty: false },
  {
    name: 'partial_arguments',
    arguments: { ...toolExcerpt('{"path":'), offset_bytes: '10', total_bytes: '18' },
    empty: false,
  },
  { name: 'array_arguments', arguments: toolExcerpt('[]'), empty: false },
  { name: 'empty_arguments', arguments: toolExcerpt('{}'), empty: true },
].map((example) => ({
  ...example,
  tool: { ...toolExample(example.name, {}, {})[0], arguments: example.arguments },
}))

export const partialApproval = {
  type: 'tool_approval_decision' as const,
  tool_name: 'unsandboxed_exec',
  decision: 'deny' as const,
  actor: { type: 'policy' as const },
  rationale: {
    ...toolExcerpt('Command is outside the workspace'),
    offset_bytes: '512',
    total_bytes: '544',
  },
  request_id: '00000000-0000-7000-8000-000000000001',
  turn_id: '00000000-0000-7000-8000-000000000002',
  approval_judge_escalated: false,
}

export const emptyStructuredRead = toolExample(
  'file_read',
  {},
  { status: 'structured', body: {}, truncated: false, cursor: null },
)[1]

export const scalarStructuredRead = toolExample(
  'file_read',
  {},
  { status: 'structured', body: 'x'.repeat(4001), truncated: false, cursor: null },
)[1]

export const emptyTextStructuredRead = toolExample(
  'file_read',
  {},
  { status: 'structured', body: '', truncated: false, cursor: null },
)[1]
