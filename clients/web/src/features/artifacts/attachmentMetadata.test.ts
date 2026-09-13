import { describe, expect, it } from 'vitest'
import { attachmentDescriptorMediaType } from './attachmentMetadata'

describe('attachment descriptor media types', () => {
  it.each(['garbage', 'image/', 'text/plain; charset', 'image/png extra', null, undefined])(
    'uses binary delivery for %s',
    (label) => {
      expect(attachmentDescriptorMediaType(label)).toBe('application/octet-stream')
    },
  )
  it.each([
    'image/png',
    'application/vnd.example+json',
    'text/plain; charset=utf-8',
    'text/plain; charset="utf-8"',
  ])('preserves MIME label %s', (label) => {
    expect(attachmentDescriptorMediaType(label)).toBe(label)
  })
})
