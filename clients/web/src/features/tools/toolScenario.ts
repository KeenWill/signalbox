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
): WebTimelineToolAttempt => ({
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
    result: toolExcerpt(JSON.stringify(result)),
  },
})

export const toolExamples = [
  toolExample(
    'sandboxed_exec',
    { program: 'cargo', arguments: ['check'] },
    {
      outcome: { kind: 'exited', code: 0 },
      stdout: { text: 'Finished successfully', completeness: 'complete' },
    },
  ),
  toolExample(
    'read_file',
    { path: 'src/main.rs' },
    { content: 'fn main() {\n    println!("Hello");\n}' },
  ),
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
    { title: 'Render tool calls', html_url: 'https://github.com/example/project/pull/12' },
  ),
  toolExample('custom_tool', { greeting: 'Hello' }, { answer: 'World' }),
]
