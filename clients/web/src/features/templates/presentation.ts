import type { WebTemplateWorkflowTool } from '../../generated/web-contract.mjs'

export const postureLabels = {
  auto: 'Automatic',
  delegated: 'Reviewed by an agent',
  human: 'Ask you',
}
const operationLabels: Record<string, string> = {
  workflow_list: 'List workflows',
  workflow_read: 'Read workflows',
  workflow_start: 'Start workflows',
  workflow_stop: 'Stop workflows',
  workflow_replay: 'Replay workflows',
  workflow_register: 'Register workflows',
}
export const operationLabel = (name: string): string => operationLabels[name] ?? name
export function grantLabel(tool: WebTemplateWorkflowTool): string {
  switch (tool.grant.kind) {
    case 'enabled':
      return tool.grant.enabled ? 'Enabled' : 'Disabled'
    case 'all_registrations':
      return 'All workflows'
    case 'registrations':
      return tool.grant.names.length > 0 ? tool.grant.names.join(', ') : 'No workflows'
  }
}
