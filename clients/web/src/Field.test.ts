import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { expect, it } from 'vitest'
import { Field } from './Field'

it('associates a generated label and error with the input while retaining its description', () => {
  const html = renderToStaticMarkup(
    createElement(Field, {
      label: 'Session ID',
      error: 'Enter a session identifier.',
      'aria-describedby': 'session-help',
      required: true,
    }),
  )
  const id = html.match(/<input[^>]* id="([^"]+)"/)?.[1]
  expect(id).toBeTruthy()
  expect(html).toContain(`for="${id}"`)
  expect(html).toContain(`aria-describedby="session-help ${id}-error"`)
  expect(html).toContain('aria-invalid="true"')
  expect(html).toContain(`id="${id}-error"`)
})

it('preserves select options and textarea content with the same label association', () => {
  const select = renderToStaticMarkup(
    createElement(
      Field,
      {
        as: 'select',
        id: 'template',
        label: 'Template',
        defaultValue: 'chat',
      },
      createElement('option', { value: 'chat' }, 'Chat'),
    ),
  )
  const textarea = renderToStaticMarkup(
    createElement(Field, {
      as: 'textarea',
      id: 'message',
      label: 'Message',
      defaultValue: 'Hello',
      rows: 3,
    }),
  )
  expect(select).toContain('for="template"')
  expect(select).toContain('<option value="chat" selected="">Chat</option>')
  expect(textarea).toContain('for="message"')
  expect(textarea).toContain('>Hello</textarea>')
  expect(textarea).not.toContain('aria-invalid')
})
