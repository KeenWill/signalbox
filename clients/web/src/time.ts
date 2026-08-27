export const displayUnixMilliseconds = (unixMilliseconds: string) => {
  const value = Number(unixMilliseconds)
  if (!Number.isSafeInteger(value)) return unixMilliseconds
  const date = new Date(value)
  if (!Number.isFinite(date.getTime())) return unixMilliseconds
  return date
}

export const displayUnixMicroseconds = (unixMicroseconds: string) => {
  if (!/^(0|[1-9][0-9]*)$/.test(unixMicroseconds)) return unixMicroseconds
  const milliseconds = BigInt(unixMicroseconds) / 1_000n
  if (milliseconds > BigInt(Number.MAX_SAFE_INTEGER)) return unixMicroseconds
  return displayUnixMilliseconds(milliseconds.toString())
}
