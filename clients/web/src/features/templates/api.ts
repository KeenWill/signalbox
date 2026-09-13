import {
  decodeWebApiErrorResponse,
  decodeWebTemplateDetail,
  decodeWebTemplateList,
  type WebTemplateDetail,
  type WebTemplateList,
  type WebTemplateSaveRequest,
} from '../../generated/web-contract.mjs'

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
    return decodeWebTemplateList(await this.read('/api/templates', signal))
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
