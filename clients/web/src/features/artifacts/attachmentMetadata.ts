// Descriptor requests require MIME syntax; timeline references can carry arbitrary ASCII labels.
const mimeToken = "[!#$%&'*+.^_`|~0-9A-Za-z-]+"
const mimeLabel = new RegExp(
  `^${mimeToken}/${mimeToken}(?:; *${mimeToken}=(?:${mimeToken}|"[\\x20-\\x21\\x23-\\x5b\\x5d-\\x7e]+" *))*$`,
  'u',
)

export const attachmentDescriptorMediaType = (label?: string | null): string =>
  label && mimeLabel.test(label) ? label : 'application/octet-stream'
