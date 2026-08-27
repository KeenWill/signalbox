import { describe, expect, it } from 'vitest'
import { displayUnixMicroseconds, displayUnixMilliseconds } from './time'

describe('browser timestamp display', () => {
  it('falls back for a safe integer outside the JavaScript Date range', () => {
    const outsideDateRange = '9007199254740991'

    expect(displayUnixMilliseconds(outsideDateRange)).toBe(outsideDateRange)
  })

  it('preserves microseconds while deriving a display date', () => {
    const timestamp = displayUnixMicroseconds('1724200000000123')

    expect(timestamp).toBeInstanceOf(Date)
    expect((timestamp as Date).getTime()).toBe(1724200000000)
  })
})
