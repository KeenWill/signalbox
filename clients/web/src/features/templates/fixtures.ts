import type { WebTemplateDetail, WebTemplateSummary } from '../../generated/web-contract.mjs'

// Synthetic catalog entries used only by unit and browser tests.
export const templateFixture: WebTemplateSummary = {
  name: 'code-review',
  digest: '1'.repeat(64),
  version: '1',
  model: { kind: 'alias', alias_id: '30000000-0000-4000-8000-000000000003' },
  model_label: 'Review model',
  dangerous_tool_auto_approval: false,
  workflow_tools: [
    {
      name: 'workflow_start',
      grant: { kind: 'registrations', names: ['build'] },
      posture: 'human',
    },
  ],
}

export const detailFixture: WebTemplateDetail = {
  summary: templateFixture,
  system_prompt: 'Review the change and explain material findings.',
  source_kind: 'template',
  definition_toml: `version = 1
[[templates]]
name = "code-review"
version = 1
alias = "30000000-0000-4000-8000-000000000003"
system_prompt = "Review the change and explain material findings."
dangerous_tool_auto_approval = false
[templates.workflow_tools.start]
names = ["build"]
posture = "human"
`,
}
