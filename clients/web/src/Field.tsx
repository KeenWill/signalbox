import { type ComponentPropsWithRef, type ReactNode, useId } from 'react'
import './field.css'

export type FieldProps = { label: string; error?: string } & (
  | ({ as?: 'input' } & ComponentPropsWithRef<'input'>)
  | ({ as: 'select' } & ComponentPropsWithRef<'select'>)
  | ({ as: 'textarea' } & ComponentPropsWithRef<'textarea'>)
)

export function Field({ label, error, id, ...props }: FieldProps) {
  const generatedId = useId()
  const controlId = id ?? generatedId
  const errorId = `${controlId}-error`
  const common = {
    id: controlId,
    'aria-invalid': error ? true : props['aria-invalid'],
    'aria-describedby':
      [props['aria-describedby'], error ? errorId : undefined].filter(Boolean).join(' ') ||
      undefined,
  } as const
  let control: ReactNode
  if (props.as === 'select') {
    const { as: _, ...native } = props
    control = <select {...native} {...common} />
  } else if (props.as === 'textarea') {
    const { as: _, ...native } = props
    control = <textarea {...native} {...common} />
  } else {
    const { as: _, ...native } = props
    control = <input {...native} {...common} />
  }
  return (
    <div className="field">
      <label htmlFor={controlId}>{label}</label>
      {control}
      {error && (
        <p className="field-error" id={errorId}>
          {error}
        </p>
      )}
    </div>
  )
}
