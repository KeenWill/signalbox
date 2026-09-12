import { useLocation, useNavigate } from '@tanstack/react-router'
import { TemplatesSurface } from './TemplatesSurface'

export function TemplatesRoute() {
  const hash = useLocation({ select: (location) => location.hash })
  const navigate = useNavigate()
  let selectedName: string | undefined
  try {
    selectedName = hash ? decodeURIComponent(hash) : undefined
  } catch {
    selectedName = hash
  }
  return (
    <TemplatesSurface
      selectedName={selectedName}
      onSelect={(name) => {
        void navigate({
          to: '/$surface',
          params: { surface: 'templates' },
          hash: name === undefined ? '' : encodeURIComponent(name),
        })
      }}
    />
  )
}
