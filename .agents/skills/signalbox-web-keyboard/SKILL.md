______________________________________________________________________

## name: signalbox-web-keyboard description: Add Signalbox commands, modal Vim-inspired navigation, focus behavior, hotkeys, and command-palette integration without bypassing accessibility or text editing.

# Signalbox web keyboard interaction

Use this skill whenever adding an action, navigation behavior, focus transition,
hotkey, sequence, command palette entry, menu item, or keyboard test.

## Command first

An application action is one registered command with:

- stable identity;
- title, description, and category;
- scope and availability predicate;
- one execution path;
- default bindings; and
- optional menu, button, or palette presentation.

Buttons, menus, TanStack Hotkeys, and the command palette invoke that command.
Do not put business behavior in a component-local key handler.

## Modes and scopes

Use a small explicit interaction state:

- **Normal** for application navigation and Vim-like sequences.
- **Insert/editing** while a text input, editor, search field, or composer owns
  text entry.
- **Palette/dialog/sheet** scopes that temporarily capture only their commands.

Do not intercept ordinary typing or browser/editor conventions inside editable
content. `Escape` unwinds the closest transient or editing context first, then
returns focus to the owning surface. It must not unpredictably jump across the
application.

## Navigation grammar

Prefer familiar Vim-inspired concepts where they fit:

- `j` and `k` move a row or timeline selection;
- `gg` and `G` reach first and latest/end;
- bracket sequences move among related domain objects;
- `/` searches the current surface;
- `?` opens keyboard help;
- `Enter` opens or activates;
- `Space` previews or inspects; and
- `Mod+K` opens the command palette.

The exact map remains centrally declared and can evolve. Architecture must
permit later remapping even when the first UI ships fixed defaults.

## Focus

- Every navigable surface has one clear focus entry and selected item.
- Selection and DOM focus are related deliberately, not accidentally.
- Virtualized items restore focus or selection by stable domain identity.
- Opening an inspector, sheet, dialog, or route records a sensible return
  target.
- Focus remains visible in compact and dark layouts.
- Pointer and touch operation update the same command/selection state.

## Accessibility

Use native elements and accessible names. Hotkeys supplement semantic controls;
they never replace them. A screen reader or Playwright role locator must be able
to discover the same action without knowing its shortcut.

## Tests

Add deterministic Playwright coverage for:

- command availability by scope;
- sequences and conflict resolution;
- typing inside editable fields;
- Escape unwinding;
- focus restoration across virtualized navigation and overlays;
- visible help/palette binding discovery; and
- primary workflow completion without a mouse.
