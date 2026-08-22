import { useCallback, useState } from 'react'

export type RemoteMediaPolicy = 'ask' | 'block' | 'allow'

export const REMOTE_MEDIA_PREFERENCE_KEY = 'signalbox.web.artifacts.remote-media.v1'
export const DEFAULT_REMOTE_MEDIA_POLICY: RemoteMediaPolicy = 'ask'

export const decodeRemoteMediaPolicy = (value: unknown): RemoteMediaPolicy =>
  value === 'block' || value === 'allow' || value === 'ask' ? value : DEFAULT_REMOTE_MEDIA_POLICY

const loadRemoteMediaPolicy = (): RemoteMediaPolicy => {
  if (typeof localStorage === 'undefined') return DEFAULT_REMOTE_MEDIA_POLICY
  return decodeRemoteMediaPolicy(localStorage.getItem(REMOTE_MEDIA_PREFERENCE_KEY))
}

export const useRemoteMediaPreference = (): readonly [
  RemoteMediaPolicy,
  (policy: RemoteMediaPolicy) => void,
] => {
  const [policy, setPolicy] = useState(loadRemoteMediaPolicy)
  const persistPolicy = useCallback((next: RemoteMediaPolicy) => {
    setPolicy(next)
    localStorage.setItem(REMOTE_MEDIA_PREFERENCE_KEY, next)
  }, [])
  return [policy, persistPolicy] as const
}
