import { useCallback, useState } from 'react'

export type RemoteMediaPolicy = 'ask' | 'block' | 'allow'

export const REMOTE_MEDIA_PREFERENCE_KEY = 'signalbox.web.artifacts.remote-media.v1'
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

const loadRemoteMediaPolicy = (): RemoteMediaPolicy => {
  if (typeof localStorage === 'undefined') return DEFAULT_REMOTE_MEDIA_POLICY
  try {
    return decodeRemoteMediaPolicy(localStorage.getItem(REMOTE_MEDIA_PREFERENCE_KEY))
  } catch {
    return DEFAULT_REMOTE_MEDIA_POLICY
  }
}

export const useRemoteMediaPreference = (): readonly [
  RemoteMediaPolicy,
  (policy: RemoteMediaPolicy) => void,
] => {
  const [policy, setPolicy] = useState(loadRemoteMediaPolicy)
  const persistPolicy = useCallback((next: RemoteMediaPolicy) => {
    setPolicy(next)
    try {
      localStorage.setItem(REMOTE_MEDIA_PREFERENCE_KEY, next)
    } catch {
      // The in-memory policy still applies when browser storage is unavailable.
    }
  }, [])
  return [policy, persistPolicy] as const
}
