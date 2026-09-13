import {
  decodeWebApiErrorResponse,
  decodeWebTemplateDetail,
  decodeWebTemplateList,
  type WebTemplateDetail,
  type WebTemplateList,
  type WebTemplateSaveRequest,
} from '../../generated/web-contract.mjs'

import { readBoundedJson } from '../../product'

// Hard safety ceiling: bounds retained catalog entries and mounted picker options.
export const MAX_TEMPLATE_LIST_ITEMS = 100

export interface TemplateApi {
  list(signal?: AbortSignal): Promise<WebTemplateList>
  detail(name: string, signal?: AbortSignal): Promise<WebTemplateDetail>
  save(name: string, request: WebTemplateSaveRequest): Promise<WebTemplateDetail>
}

export class HttpTemplateApi implements TemplateApi {
  constructor(private readonly request: typeof fetch = (input, init) => fetch(input, init)) {}

  private async read(path: string, signal?: AbortSignal): Promise<unknown> {
    const response = await this.request(path, {
      signal,
      credentials: 'same-origin',
      headers: { Accept: 'application/json' },
    })
    const value: unknown = await response.json()
    if (!response.ok) throw new Error(decodeWebApiErrorResponse(value).error.message)
    return value
  }

  async list(signal?: AbortSignal): Promise<WebTemplateList> {
    const response = await this.request('/api/templates', {
      signal,
      credentials: 'same-origin',
      headers: { Accept: 'application/json' },
    })
    const value = await readBoundedJson(response)
    if (!response.ok) throw new Error(decodeWebApiErrorResponse(value).error.message)
    if (
      typeof value === 'object' &&
      value !== null &&
      'templates' in value &&
      Array.isArray(value.templates) &&
      value.templates.length > MAX_TEMPLATE_LIST_ITEMS
    )
      throw new Error('Template catalog exceeded the item limit.')
    return decodeWebTemplateList(value)
  }

  async detail(name: string, signal?: AbortSignal): Promise<WebTemplateDetail> {
    const detail = decodeWebTemplateDetail(
      await this.read(`/api/templates/${encodeURIComponent(name)}`, signal),
    )
    if (detail.summary.name !== name) throw new Error('The returned template does not match.')
    return detail
  }
  async save(name: string, request: WebTemplateSaveRequest): Promise<WebTemplateDetail> {
    const response = await this.request(`/api/templates/${encodeURIComponent(name)}`, {
      method: 'PUT',
      credentials: 'same-origin',
      headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
      body: JSON.stringify(request),
    })
    const value: unknown = await response.json()
    if (!response.ok) throw new Error(decodeWebApiErrorResponse(value).error.message)
    const detail = decodeWebTemplateDetail(value)
    if (detail.summary.name !== name) throw new Error('The returned template does not match.')
    return detail
  }
}
