export type RemoteMediaPolicy = 'ask' | 'block' | 'allow'

export const DEFAULT_REMOTE_MEDIA_POLICY: RemoteMediaPolicy = 'ask'

export const decodeRemoteMediaPolicy = (value: unknown): RemoteMediaPolicy =>
  value === 'block' || value === 'allow' || value === 'ask' ? value : DEFAULT_REMOTE_MEDIA_POLICY

export const admitRemoteMediaUrl = (value: string): string | null => {
  try {
    const url = new URL(value)
    return url.protocol === 'https:' && url.username === '' && url.password === '' ? url.href : null
  } catch {
    return null
  }
}
