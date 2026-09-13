// Descriptor requests require MIME syntax; timeline references can carry arbitrary ASCII labels.
const mimeToken = "[!#$%&'*+.^_`|~0-9A-Za-z-]+"
const mimeQuotedValue = '"(?:[\\x20-\\x21\\x23-\\x5b\\x5d-\\x7e]|\\x5c[\\x20-\\x21\\x23-\\x7e])+"'
const mimeLabel = new RegExp(
  `^${mimeToken}/${mimeToken}(?:; *${mimeToken}=(?:${mimeToken}|${mimeQuotedValue}) *)*$`,
  'u',
)

export const attachmentDescriptorMediaType = (label?: string | null): string =>
  label && mimeLabel.test(label) ? label : 'application/octet-stream'
