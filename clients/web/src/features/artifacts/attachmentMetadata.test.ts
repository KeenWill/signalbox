import { describe, expect, it } from 'vitest'
import { attachmentTypeLabel } from '../../ArtifactInspector'
import { attachmentDescriptorMediaType } from './attachmentMetadata'

describe('attachment descriptor media types', () => {
  it.each([
    'garbage',
    String.raw`image/png;x="abc\"`,
    'image/png;',
    'image/png; ',
    'text/plain; charset=utf-8;',
    'image/',
    'text/plain; charset',
    'image/png extra',
    null,
    undefined,
  ])('uses binary delivery for %s', (label) => {
    expect(attachmentDescriptorMediaType(label)).toBe('application/octet-stream')
    expect(attachmentTypeLabel(label)).toBe('File')
  })
  it.each([
    'image/png',
    String.raw`image/png;x="a\z"`,
    String.raw`image/png;x="a\\z"`,
    'application/vnd.example+json',
    'text/plain; charset=utf-8',
    'text/plain; charset=utf-8   ',
    'text/plain; charset="utf-8"',
  ])('preserves MIME label %s', (label) => {
    expect(attachmentDescriptorMediaType(label)).toBe(label)
  })
})

it.each([
  ['IMAGE/PNG', 'Image'],
  ['Audio/ogg', 'Audio'],
  ['VIDEO/MP4', 'Video'],
  ['APPLICATION/PDF', 'PDF'],
  ['application/pdf;version=1.7', 'PDF'],
  ['APPLICATION/PDF; version=1.7', 'PDF'],
  ['TEXT/PLAIN', 'Text file'],
])('labels MIME type %s case-insensitively', (mediaType, label) => {
  expect(attachmentTypeLabel(mediaType)).toBe(label)
})
