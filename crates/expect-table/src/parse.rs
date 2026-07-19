//! Hand-written recursive-descent parser for the derived-`Debug` grammar.
//!
//! The parser never fails: any region the grammar does not cover degrades to
//! one [`Value::Atom`] leaf holding the raw text, scoped as locally as
//! possible — an unparseable field value degrades alone while the rest of its
//! struct still parses. Degradation consumes a balanced region (respecting
//! parentheses, brackets, braces, best-effort angle brackets, and
//! string/char literals with escapes), so a custom `Debug` impl renders
//! verbatim — interior commas included — instead of derailing its neighbors.
//!
//! Bare-braced regions are collection `Debug` output: `{k: v, …}` parses
//! as a map and `{a, …}` as a set (its entries carry no `: `); a braced
//! region following neither grammar degrades verbatim like any other
//! unrecognized region. Entries keep parse order in the tree — rendering
//! owns the sorted-entry normalization the crate docs state.
//!
//! A depth-zero comma is context-sensitive. In a struct body it ends the
//! current element only when what follows looks like the field grammar —
//! `identifier:` (not `::`, which is a path), the `..` non-exhaustive
//! marker, or the closing `}`; otherwise the comma came from a custom
//! `Debug` leaf and belongs to the atom. In tuples and lists a depth-zero
//! comma always separates items: no field-boundary signal exists there, so
//! a comma-printing custom leaf splits — the best-effort asymmetry the
//! crate docs state.

/// A value parsed from derived-`Debug` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Value {
    /// `Name { field: value, .. }` — a struct or a struct enum variant.
    ///
    /// The parser guarantees `fields` is nonempty; a bare name parses as an
    /// [`Value::Atom`] instead (as does a fieldless non-exhaustive body,
    /// `Name { .. }`, kept verbatim).
    Struct {
        name: String,
        fields: Vec<(String, Value)>,
        /// `true` when the body ended with the `..` marker
        /// (`finish_non_exhaustive`); compact rendering keeps the marker
        /// so the cell never looks falsely exhaustive.
        non_exhaustive: bool,
    },
    /// `Name(a, b)` — a tuple struct or tuple enum variant — or, with no
    /// name, a plain `(a, b)` tuple.
    Tuple {
        name: Option<String>,
        items: Vec<Value>,
    },
    /// `[a, b]`.
    List(Vec<Value>),
    /// `{key: value, …}` — map `Debug` output (`BTreeMap`, `HashMap`).
    /// Entries hold parse order here; rendering sorts them by rendered
    /// key text so `HashMap` iteration order never reaches a snapshot
    /// (the crate docs' determinism contract). An empty `{}` is
    /// indistinguishable from an empty set and parses as an empty map.
    Map(Vec<(Value, Value)>),
    /// `{a, …}` — set `Debug` output (`BTreeSet`, `HashSet`), told from
    /// a map by its entries carrying no `: `. Rendering sorts entries by
    /// their rendered text.
    Set(Vec<Value>),
    /// `"…"` — the escaped interior of a string literal, quotes stripped.
    Str(String),
    /// `'…'` — the escaped interior of a char literal, quotes stripped.
    Char(String),
    /// Unit variants, numbers, booleans, and every unparseable region.
    Atom(String),
}

/// Parses one complete derived-`Debug` rendering. Never fails: input the
/// grammar does not cover comes back as an atomic leaf.
pub(crate) fn parse(text: &str) -> Value {
    let mut parser = Parser { text, pos: 0 };
    let value = parser.value(Context::ItemList);
    parser.skip_whitespace();
    if parser.pos < parser.text.len() {
        // Trailing input the grammar does not cover: the whole rendering is
        // one atomic leaf.
        return Value::Atom(text.trim().to_string());
    }
    value
}

/// Where an element sits, deciding what a depth-zero comma means while it
/// is parsed or degraded (see the module docs).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Context {
    /// Inside `{ … }`: a comma separates fields only when a field
    /// boundary follows; otherwise it belongs to a degraded leaf.
    StructBody,
    /// Inside `( … )` or `[ … ]`, or at the top level: a depth-zero
    /// comma always separates items.
    ItemList,
    /// A map key inside `{ … }`: as [`Context::ItemList`], except a
    /// depth-zero `:` (never the `::` of a path) also ends the element.
    MapKey,
}

struct Parser<'text> {
    text: &'text str,
    pos: usize,
}

impl Parser<'_> {
    fn rest(&self) -> &str {
        &self.text[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) {
        if let Some(next) = self.peek() {
            self.pos += next.len_utf8();
        }
    }

    fn eat(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(next) if next.is_whitespace()) {
            self.bump();
        }
    }

    fn value(&mut self, context: Context) -> Value {
        self.skip_whitespace();
        let start = self.pos;
        match self.peek() {
            None => Value::Atom(String::new()),
            Some('"') => self.string_literal(),
            // A lifetime apostrophe (`'_`, `'a`) falls through to the
            // degraded-atom arm and stays plain token text.
            Some('\'') if self.apostrophe_opens_char_literal() => self.char_literal(),
            Some('(') => match self.items(')') {
                Some(items) => Value::Tuple { name: None, items },
                None => self.degrade(start, context),
            },
            Some('[') => match self.items(']') {
                Some(items) => Value::List(items),
                None => self.degrade(start, context),
            },
            Some('{') => match self.braced_collection() {
                Some(value) => value,
                None => self.degrade(start, context),
            },
            Some(next) if next.is_alphabetic() || next == '_' => self.named(start, context),
            _ => self.degrade(start, context),
        }
    }

    /// A value opening with an identifier: a struct, a tuple struct or
    /// variant, or a bare atom (unit variant, boolean, or word).
    fn named(&mut self, start: usize, context: Context) -> Value {
        let name = self.ident();
        let after_ident = self.pos;
        self.skip_whitespace();
        match self.peek() {
            Some('{') => match self.struct_fields() {
                // A fieldless non-exhaustive body (`Secret { .. }`) keeps
                // its marker verbatim as a leaf; only a genuinely empty
                // body collapses to the bare name.
                Some((fields, non_exhaustive)) if fields.is_empty() => {
                    if non_exhaustive {
                        Value::Atom(self.text[start..self.pos].trim().to_string())
                    } else {
                        Value::Atom(name)
                    }
                }
                Some((fields, non_exhaustive)) => Value::Struct {
                    name,
                    fields,
                    non_exhaustive,
                },
                None => self.degrade(start, context),
            },
            Some('(') => match self.items(')') {
                Some(items) => Value::Tuple {
                    name: Some(name),
                    items,
                },
                None => self.degrade(start, context),
            },
            _ => {
                self.pos = after_ident;
                Value::Atom(name)
            }
        }
    }

    fn ident(&mut self) -> String {
        let start = self.pos;
        while matches!(self.peek(), Some(next) if next.is_alphanumeric() || next == '_') {
            self.bump();
        }
        self.text[start..self.pos].to_string()
    }

    /// `field: value` pairs after a struct's `{`, accepting a trailing `..`
    /// marker (`finish_non_exhaustive`), reported in the returned flag.
    /// Returns `None` when the braced region does not follow the field
    /// grammar; the caller degrades it.
    fn struct_fields(&mut self) -> Option<(Vec<(String, Value)>, bool)> {
        self.bump(); // '{'
        let mut fields = Vec::new();
        loop {
            self.skip_whitespace();
            if self.eat('}') {
                return Some((fields, false));
            }
            if self.rest().starts_with("..") {
                self.pos += 2;
                self.skip_whitespace();
                return self.eat('}').then_some((fields, true));
            }
            let name = self.ident();
            if name.is_empty() {
                return None;
            }
            self.skip_whitespace();
            if !self.eat(':') {
                return None;
            }
            fields.push((name, self.element(Context::StructBody)));
            self.skip_whitespace();
            if self.eat(',') {
                continue;
            }
            if self.eat('}') {
                return Some((fields, false));
            }
            return None;
        }
    }

    /// Comma-separated values after a `(` or `[`, up to `closer`. Returns
    /// `None` when the region does not close as expected; the caller
    /// degrades it.
    fn items(&mut self, closer: char) -> Option<Vec<Value>> {
        self.bump(); // opener
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.eat(closer) {
            return Some(items);
        }
        loop {
            items.push(self.element(Context::ItemList));
            self.skip_whitespace();
            if self.eat(',') {
                self.skip_whitespace();
                // A closer directly after a comma is derived `Debug`'s
                // trailing comma marking a singleton tuple (`(1,)`), not
                // an empty extra item.
                if self.eat(closer) {
                    return Some(items);
                }
                continue;
            }
            if self.eat(closer) {
                return Some(items);
            }
            return None;
        }
    }

    /// A bare-braced region at value position: map (`{k: v, …}`) or set
    /// (`{a, …}`) `Debug` output, the only derived-`Debug` forms opening
    /// with an unnamed `{`. Returns `None` when the region follows
    /// neither grammar — a custom `Debug` leaf printing braces — and the
    /// caller degrades it verbatim.
    fn braced_collection(&mut self) -> Option<Value> {
        let start = self.pos;
        if let Some(entries) = self.map_entries() {
            return Some(Value::Map(entries));
        }
        self.pos = start;
        self.set_entries().map(Value::Set)
    }

    /// `key: value` entries after a map's `{`. Returns `None` when the
    /// region is not map-shaped; empty braces parse as an empty map.
    fn map_entries(&mut self) -> Option<Vec<(Value, Value)>> {
        self.bump(); // '{'
        let mut entries = Vec::new();
        self.skip_whitespace();
        if self.eat('}') {
            return Some(entries);
        }
        loop {
            let key = self.map_key()?;
            self.bump(); // the ':' `map_key` stopped on
            let value = self.element(Context::ItemList);
            if value == Value::Atom(String::new()) {
                return None;
            }
            entries.push((key, value));
            self.skip_whitespace();
            if self.eat(',') {
                continue;
            }
            if self.eat('}') {
                return Some(entries);
            }
            return None;
        }
    }

    /// One map key: a value ending at a depth-zero `:` (never the `::`
    /// of a path, which belongs to a degraded leaf). Returns `None` —
    /// the region was not a map after all — when no such boundary
    /// follows, leaving the scanner wherever the caller resets it.
    fn map_key(&mut self) -> Option<Value> {
        self.skip_whitespace();
        let start = self.pos;
        let mut key = self.value(Context::MapKey);
        self.skip_whitespace();
        if !self.scanner_on_entry_colon() {
            key = self.degrade(start, Context::MapKey);
            self.skip_whitespace();
            if !self.scanner_on_entry_colon() {
                return None;
            }
        }
        (key != Value::Atom(String::new())).then_some(key)
    }

    /// Whether the scanner sits on a map entry's `:` — one colon, not a
    /// `::` path separator.
    fn scanner_on_entry_colon(&self) -> bool {
        self.rest().starts_with(':') && !self.rest().starts_with("::")
    }

    /// Comma-separated entries after a set's `{`. Returns `None` when
    /// the region does not close as a set; an entry degrading to nothing
    /// — consecutive commas from a hostile leaf, say — also rejects the
    /// region, so it stays one verbatim atom rather than losing text.
    fn set_entries(&mut self) -> Option<Vec<Value>> {
        self.bump(); // '{'
        let mut entries = Vec::new();
        self.skip_whitespace();
        if self.eat('}') {
            return Some(entries);
        }
        loop {
            let entry = self.element(Context::ItemList);
            if entry == Value::Atom(String::new()) {
                return None;
            }
            entries.push(entry);
            self.skip_whitespace();
            if self.eat(',') {
                continue;
            }
            if self.eat('}') {
                return Some(entries);
            }
            return None;
        }
    }

    /// One element of a field, tuple, or list: a value that must end at a
    /// separator or closer. When it does not — a custom `Debug` impl printed
    /// something the grammar does not cover — the whole element degrades to
    /// one atom, leaving the enclosing structure parseable. In a struct
    /// body a comma is a separator only when a field boundary follows it;
    /// a bare comma from a custom `Debug` leaf belongs to the element.
    fn element(&mut self, context: Context) -> Value {
        self.skip_whitespace();
        let start = self.pos;
        let value = self.value(context);
        self.skip_whitespace();
        match self.peek() {
            None | Some(')' | ']' | '}') => value,
            Some(',') if context == Context::ItemList || self.comma_separates_struct_fields() => {
                value
            }
            _ => self.degrade(start, context),
        }
    }

    /// With the scanner on a depth-zero comma inside a struct body,
    /// decides whether the comma separates fields: it does when what
    /// follows (after whitespace) is the closing `}`, the `..`
    /// non-exhaustive marker, or a field boundary — an identifier
    /// followed by `:` but not `::` (a path such as `core::result`
    /// belongs to a degraded leaf, not the field grammar). Anything else
    /// means a custom `Debug` leaf printed the comma, and it belongs to
    /// the current element.
    fn comma_separates_struct_fields(&self) -> bool {
        let after_comma = self.rest()[1..].trim_start();
        if after_comma.starts_with('}') || after_comma.starts_with("..") {
            return true;
        }
        if !matches!(after_comma.chars().next(), Some(first) if first.is_alphabetic() || first == '_')
        {
            return false;
        }
        let ident_end = after_comma
            .find(|next: char| !(next.is_alphanumeric() || next == '_'))
            .unwrap_or(after_comma.len());
        let after_ident = after_comma[ident_end..].trim_start();
        after_ident.starts_with(':') && !after_ident.starts_with("::")
    }

    /// With the scanner on a `'`, decides whether it opens a genuine char
    /// literal: one character or one `\`-escape followed by the closing
    /// `'`. A lifetime — `'_`, `'a`, `'static`, as lifetime-bearing type
    /// names such as `fn(&'_ str) -> &'_ str` contain — has no closing
    /// apostrophe there and lexes as plain token text instead, so it
    /// cannot consume across delimiters and unbalance the scan.
    fn apostrophe_opens_char_literal(&self) -> bool {
        let mut chars = self.rest().chars();
        chars.next(); // the apostrophe itself
        match chars.next() {
            // `'\n'`, `'\''`, `'\u{7f}'`: a backslash directly after the
            // opening quote only ever comes from a char literal's escape.
            Some('\\') => true,
            // `'x'` closes directly after one character; a lifetime
            // (`'a `, `'_>`) does not.
            Some(_) => chars.next() == Some('\''),
            None => false,
        }
    }

    /// `"…"` with `\`-escapes; an unterminated literal takes the rest.
    fn string_literal(&mut self) -> Value {
        self.bump(); // opening quote
        let body = self.literal_body('"');
        Value::Str(body)
    }

    /// `'…'` with `\`-escapes; an unterminated literal takes the rest.
    fn char_literal(&mut self) -> Value {
        self.bump(); // opening quote
        let body = self.literal_body('\'');
        Value::Char(body)
    }

    fn literal_body(&mut self, terminator: char) -> String {
        let start = self.pos;
        while let Some(next) = self.peek() {
            if next == '\\' {
                self.bump();
                self.bump();
                continue;
            }
            if next == terminator {
                let body = self.text[start..self.pos].to_string();
                self.bump();
                return body;
            }
            self.bump();
        }
        self.text[start..].to_string()
    }

    /// Restarts at `start` and consumes one balanced element as an atom.
    fn degrade(&mut self, start: usize, context: Context) -> Value {
        self.pos = start;
        self.consume_balanced_element(context);
        Value::Atom(self.text[start..self.pos].trim_end().to_string())
    }

    /// Consumes text until, at nesting depth zero, the next character is a
    /// separator (`,`) or a closer belonging to an enclosing region — or the
    /// input ends. In a struct-body context a depth-zero comma is a
    /// separator only when a field boundary or the closer follows it
    /// ([`Parser::comma_separates_struct_fields`]); otherwise the comma
    /// belongs to the atom and the scan continues. String and char
    /// literals are skipped escape-aware so their delimiters and commas
    /// cannot unbalance the scan; a lifetime apostrophe
    /// ([`Parser::apostrophe_opens_char_literal`]) is plain token text,
    /// so `fn(&'_ str) -> &'_ str` cannot swallow its `)` into a bogus
    /// char literal.
    ///
    /// Angle brackets balance best-effort so type names like
    /// `PhantomData<(u32, u32)>` or `PhantomData<Result<u32, u32>>` stay one
    /// atom instead of ending at an interior comma: `<` opens a
    /// generic-argument list only directly after an identifier character
    /// (`Vec<`), never after anything else (`a < b`). Angle depth is a
    /// hint, and hard boundaries win over it: an enclosing closer at depth
    /// zero still ends the atom even inside an unclosed `<`, and in a
    /// struct body a depth-zero comma followed by a recognized field
    /// boundary ends the atom too — an unmatched `<` from a custom leaf
    /// (`a<b`) was a plain less-than after all and must not swallow a
    /// genuine `, count: 4` sibling. Genuine generic nesting is safe from
    /// that lookahead: inside `Result<u32, u32>` the comma is followed by
    /// a value-shaped token (`u32>`), never by `identifier:`.
    fn consume_balanced_element(&mut self, context: Context) {
        let mut depth = 0usize;
        let mut angle_depth = 0usize;
        let mut previous: Option<char> = None;
        while let Some(next) = self.peek() {
            match next {
                '"' => {
                    self.bump();
                    self.literal_body('"');
                }
                // A lifetime apostrophe (`'_`, `'a`) is plain token text;
                // only a genuine char literal skips escape-aware.
                '\'' if self.apostrophe_opens_char_literal() => {
                    self.bump();
                    self.literal_body('\'');
                }
                '(' | '[' | '{' => {
                    depth += 1;
                    self.bump();
                }
                ')' | ']' | '}' => {
                    if depth == 0 {
                        return;
                    }
                    depth -= 1;
                    self.bump();
                }
                '<' if matches!(previous, Some(prior) if prior.is_alphanumeric() || prior == '_') =>
                {
                    angle_depth += 1;
                    self.bump();
                }
                '>' if angle_depth > 0 => {
                    angle_depth -= 1;
                    self.bump();
                }
                // A map key ends at a depth-zero entry colon; the `::` of
                // a degraded path leaf stays in the atom.
                ':' if depth == 0 && context == Context::MapKey => {
                    if self.rest().starts_with("::") {
                        self.pos += 2;
                    } else {
                        return;
                    }
                }
                ',' if depth == 0 => {
                    let separates = match context {
                        // No field-boundary signal exists in item lists
                        // (or map keys), so only the angle-depth hint
                        // disambiguates: a depth-zero comma outside `<…>`
                        // separates items.
                        Context::ItemList | Context::MapKey => angle_depth == 0,
                        // A recognized field boundary is a hard boundary
                        // and wins over the angle-depth hint, even under
                        // an unclosed `<` from a custom leaf (`a<b`).
                        Context::StructBody => self.comma_separates_struct_fields(),
                    };
                    if separates {
                        return;
                    }
                    // A custom `Debug` leaf printed this comma, or it sits
                    // inside a generic-argument list; it stays in the atom.
                    self.bump();
                }
                _ => self.bump(),
            }
            previous = Some(next);
        }
    }
}

#[cfg(test)]
#[allow(
    dead_code,
    reason = "grammar fixtures are read only through their Debug derives"
)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::marker::PhantomData;

    use super::{Value, parse};

    fn atom(text: &str) -> Value {
        Value::Atom(text.to_string())
    }

    fn field(name: &str, value: Value) -> (String, Value) {
        (name.to_string(), value)
    }

    #[derive(Debug)]
    struct Position {
        x: i32,
        y: i32,
    }

    #[derive(Debug)]
    struct Placed {
        label: &'static str,
        origin: Position,
    }

    #[test]
    fn nested_struct_parses_field_by_field() {
        let placed = Placed {
            label: "start",
            origin: Position { x: -3, y: 40 },
        };

        assert_eq!(
            parse(&format!("{placed:?}")),
            Value::Struct {
                name: "Placed".to_string(),
                non_exhaustive: false,
                fields: vec![
                    field("label", Value::Str("start".to_string())),
                    field(
                        "origin",
                        Value::Struct {
                            name: "Position".to_string(),
                            non_exhaustive: false,
                            fields: vec![field("x", atom("-3")), field("y", atom("40"))],
                        }
                    ),
                ],
            }
        );
    }

    #[derive(Debug)]
    enum Shape {
        Unit,
        Pair(u8, u8),
        Sized { width: u16 },
    }

    #[test]
    fn unit_enum_variant_parses_as_an_atom() {
        assert_eq!(parse(&format!("{:?}", Shape::Unit)), atom("Unit"));
    }

    #[test]
    fn tuple_enum_variant_parses_as_a_named_tuple() {
        assert_eq!(
            parse(&format!("{:?}", Shape::Pair(3, 5))),
            Value::Tuple {
                name: Some("Pair".to_string()),
                items: vec![atom("3"), atom("5")],
            }
        );
    }

    #[test]
    fn struct_enum_variant_parses_as_a_struct() {
        assert_eq!(
            parse(&format!("{:?}", Shape::Sized { width: 7 })),
            Value::Struct {
                name: "Sized".to_string(),
                non_exhaustive: false,
                fields: vec![field("width", atom("7"))],
            }
        );
    }

    #[test]
    fn nested_enum_with_struct_payload_parses_recursively() {
        assert_eq!(
            parse(&format!("{:?}", Some(Shape::Sized { width: 7 }))),
            Value::Tuple {
                name: Some("Some".to_string()),
                items: vec![Value::Struct {
                    name: "Sized".to_string(),
                    non_exhaustive: false,
                    fields: vec![field("width", atom("7"))],
                }],
            }
        );
    }

    #[test]
    fn option_and_vec_combinations_parse_recursively() {
        let values: Vec<Option<u8>> = vec![Some(2), None];

        assert_eq!(
            parse(&format!("{values:?}")),
            Value::List(vec![
                Value::Tuple {
                    name: Some("Some".to_string()),
                    items: vec![atom("2")],
                },
                atom("None"),
            ])
        );
    }

    #[test]
    fn hostile_string_content_stays_one_string_leaf() {
        let hostile = Placed {
            label: "a { b, \" c",
            origin: Position { x: 1, y: 2 },
        };

        assert_eq!(
            parse(&format!("{hostile:?}")),
            Value::Struct {
                name: "Placed".to_string(),
                non_exhaustive: false,
                fields: vec![
                    field("label", Value::Str("a { b, \\\" c".to_string())),
                    field(
                        "origin",
                        Value::Struct {
                            name: "Position".to_string(),
                            non_exhaustive: false,
                            fields: vec![field("x", atom("1")), field("y", atom("2"))],
                        }
                    ),
                ],
            }
        );
    }

    #[derive(Debug)]
    struct Empty {}

    #[derive(Debug)]
    struct Unit;

    #[test]
    fn empty_and_unit_structs_parse_as_bare_atoms() {
        assert_eq!(parse(&format!("{:?}", Empty {})), atom("Empty"));
        assert_eq!(parse(&format!("{:?}", Unit)), atom("Unit"));
    }

    #[derive(Debug)]
    struct Wrapped(u32);

    #[test]
    fn tuple_struct_and_plain_tuple_parse_as_tuples() {
        assert_eq!(
            parse(&format!("{:?}", Wrapped(9))),
            Value::Tuple {
                name: Some("Wrapped".to_string()),
                items: vec![atom("9")],
            }
        );
        assert_eq!(
            parse(&format!("{:?}", (1, "two"))),
            Value::Tuple {
                name: None,
                items: vec![atom("1"), Value::Str("two".to_string())],
            }
        );
    }

    #[test]
    fn singleton_tuple_trailing_comma_parses_as_one_item() {
        assert_eq!(
            parse(&format!("{:?}", (1,))),
            Value::Tuple {
                name: None,
                items: vec![atom("1")],
            }
        );
        assert_eq!(
            parse(&format!("{:?}", [(1,), (2,)])),
            Value::List(vec![
                Value::Tuple {
                    name: None,
                    items: vec![atom("1")],
                },
                Value::Tuple {
                    name: None,
                    items: vec![atom("2")],
                },
            ])
        );
    }

    #[test]
    fn char_and_float_literals_parse_as_leaves() {
        assert_eq!(
            parse(&format!("{:?}", ('\'', -1.5))),
            Value::Tuple {
                name: None,
                items: vec![Value::Char("\\'".to_string()), atom("-1.5")],
            }
        );
    }

    #[test]
    fn map_debug_output_parses_entry_by_entry() {
        let map = BTreeMap::from([(1, "one"), (2, "two")]);

        assert_eq!(
            parse(&format!("{map:?}")),
            Value::Map(vec![
                (atom("1"), Value::Str("one".to_string())),
                (atom("2"), Value::Str("two".to_string())),
            ])
        );
    }

    #[test]
    fn set_debug_output_parses_entry_by_entry() {
        let set = BTreeSet::from(["a", "b"]);

        assert_eq!(
            parse(&format!("{set:?}")),
            Value::Set(vec![
                Value::Str("a".to_string()),
                Value::Str("b".to_string()),
            ])
        );
    }

    struct HostileBraces;

    impl std::fmt::Debug for HostileBraces {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "{{ not, a: map")
        }
    }

    /// A braced region following neither the map nor the set grammar —
    /// here an unclosed one from a custom `Debug` leaf — stays one
    /// verbatim atom: collection parsing never turns degradation lossy.
    #[test]
    fn braced_region_matching_neither_map_nor_set_degrades_verbatim() {
        assert_eq!(parse(&format!("{HostileBraces:?}")), atom("{ not, a: map"));
    }

    struct Prose;

    impl std::fmt::Debug for Prose {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "three <> odd, tokens")
        }
    }

    #[test]
    fn custom_debug_output_degrades_to_one_atomic_leaf() {
        assert_eq!(parse(&format!("{:?}", Prose)), atom("three <> odd, tokens"));
    }

    #[derive(Debug)]
    struct HoldsCustom {
        tag: PhantomData<u32>,
        count: u8,
    }

    #[test]
    fn unparseable_field_value_degrades_alone_not_its_struct() {
        let holds = HoldsCustom {
            tag: PhantomData,
            count: 4,
        };

        assert_eq!(
            parse(&format!("{holds:?}")),
            Value::Struct {
                name: "HoldsCustom".to_string(),
                non_exhaustive: false,
                fields: vec![
                    field("tag", atom("PhantomData<u32>")),
                    field("count", atom("4")),
                ],
            }
        );
    }

    struct BareCommaLeaf;

    impl std::fmt::Debug for BareCommaLeaf {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "x, y")
        }
    }

    #[derive(Debug)]
    struct HoldsBareComma {
        before: u8,
        pair: BareCommaLeaf,
        after: u8,
    }

    #[test]
    fn struct_field_leaf_with_unbracketed_comma_degrades_alone() {
        let holds = HoldsBareComma {
            before: 1,
            pair: BareCommaLeaf,
            after: 2,
        };

        assert_eq!(
            parse(&format!("{holds:?}")),
            Value::Struct {
                name: "HoldsBareComma".to_string(),
                non_exhaustive: false,
                fields: vec![
                    field("before", atom("1")),
                    field("pair", atom("x, y")),
                    field("after", atom("2")),
                ],
            }
        );
    }

    /// Pinned-observed asymmetry, not promised: item lists carry no
    /// field-boundary signal, so a depth-zero comma from a custom `Debug`
    /// leaf splits the leaf into two items there (see the crate docs on
    /// best-effort degradation).
    #[test]
    fn list_item_leaf_with_unbracketed_comma_splits_best_effort() {
        assert_eq!(
            parse(&format!("{:?}", vec![BareCommaLeaf])),
            Value::List(vec![atom("x"), atom("y")]),
        );
    }

    struct Redacted;

    impl std::fmt::Debug for Redacted {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("Redacted")
                .field("shown", &1u8)
                .finish_non_exhaustive()
        }
    }

    #[test]
    fn non_exhaustive_marker_after_a_comma_closes_the_struct_and_sets_the_flag() {
        assert_eq!(
            parse(&format!("{:?}", Redacted)),
            Value::Struct {
                name: "Redacted".to_string(),
                non_exhaustive: true,
                fields: vec![field("shown", atom("1"))],
            }
        );
    }

    struct AngleLeaf;

    impl std::fmt::Debug for AngleLeaf {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "a<b")
        }
    }

    #[derive(Debug)]
    struct HoldsAngleLeaf {
        leaf: AngleLeaf,
        count: u8,
    }

    /// An unmatched `<` after an identifier tentatively opens an
    /// angle-bracket hint that never closes; the recognized field boundary
    /// after the comma is a hard boundary and wins, so the sibling field
    /// survives.
    #[test]
    fn struct_leaf_with_unmatched_angle_keeps_the_sibling_field() {
        let holds = HoldsAngleLeaf {
            leaf: AngleLeaf,
            count: 4,
        };

        assert_eq!(
            parse(&format!("{holds:?}")),
            Value::Struct {
                name: "HoldsAngleLeaf".to_string(),
                non_exhaustive: false,
                fields: vec![field("leaf", atom("a<b")), field("count", atom("4"))],
            }
        );
    }

    struct FullyRedacted;

    impl std::fmt::Debug for FullyRedacted {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.debug_struct("Secret").finish_non_exhaustive()
        }
    }

    /// `finish_non_exhaustive` with no shown fields prints `Secret { .. }`;
    /// the marker is meaningful Debug output and stays in the leaf instead
    /// of collapsing to a falsely unit-like `Secret`.
    #[test]
    fn fully_non_exhaustive_struct_keeps_its_marker_as_a_leaf() {
        assert_eq!(
            parse(&format!("{:?}", FullyRedacted)),
            atom("Secret { .. }")
        );
    }

    #[derive(Debug)]
    struct HoldsLifetimes {
        check: PhantomData<for<'a> fn(&'a str) -> &'a str>,
        count: u8,
    }

    /// Lifetimes in degraded type names (`fn(&'_ str)`) lex as plain
    /// token text: an apostrophe not closed immediately after one
    /// character or escape is no char-literal opener, so it cannot
    /// consume across delimiters and collapse the struct to one atom.
    #[test]
    fn lifetime_bearing_leaf_degrades_alone_not_its_struct() {
        let holds = HoldsLifetimes {
            check: PhantomData,
            count: 4,
        };

        assert_eq!(
            parse(&format!("{holds:?}")),
            Value::Struct {
                name: "HoldsLifetimes".to_string(),
                non_exhaustive: false,
                fields: vec![
                    field("check", atom("PhantomData<fn(&'_ str) -> &'_ str>")),
                    field("count", atom("4")),
                ],
            }
        );
    }

    #[derive(Debug)]
    struct HoldsCommaGenerics {
        pair: PhantomData<(u32, u32)>,
        nested: PhantomData<Result<u32, u32>>,
        count: u8,
    }

    #[test]
    fn degraded_atom_keeps_commas_inside_its_own_brackets() {
        let holds = HoldsCommaGenerics {
            pair: PhantomData,
            nested: PhantomData,
            count: 4,
        };

        assert_eq!(
            parse(&format!("{holds:?}")),
            Value::Struct {
                name: "HoldsCommaGenerics".to_string(),
                non_exhaustive: false,
                fields: vec![
                    field("pair", atom("PhantomData<(u32, u32)>")),
                    field(
                        "nested",
                        atom("PhantomData<core::result::Result<u32, u32>>"),
                    ),
                    field("count", atom("4")),
                ],
            }
        );
    }
}
