//! Hand-written recursive-descent parser for the derived-`Debug` grammar.
//!
//! The parser never fails: any region the grammar does not cover degrades to
//! one [`Value::Atom`] leaf holding the raw text, scoped as locally as
//! possible — an unparseable field value degrades alone while the rest of its
//! struct still parses. Degradation consumes a balanced region (respecting
//! parentheses, brackets, braces, and string/char literals with escapes), so
//! a custom `Debug` impl renders verbatim instead of derailing its neighbors.

/// A value parsed from derived-`Debug` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Value {
    /// `Name { field: value, .. }` — a struct or a struct enum variant.
    ///
    /// The parser guarantees `fields` is nonempty; a bare name parses as an
    /// [`Value::Atom`] instead.
    Struct {
        name: String,
        fields: Vec<(String, Value)>,
    },
    /// `Name(a, b)` — a tuple struct or tuple enum variant — or, with no
    /// name, a plain `(a, b)` tuple.
    Tuple {
        name: Option<String>,
        items: Vec<Value>,
    },
    /// `[a, b]`.
    List(Vec<Value>),
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
    let value = parser.value();
    parser.skip_whitespace();
    if parser.pos < parser.text.len() {
        // Trailing input the grammar does not cover: the whole rendering is
        // one atomic leaf.
        return Value::Atom(text.trim().to_string());
    }
    value
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

    fn value(&mut self) -> Value {
        self.skip_whitespace();
        let start = self.pos;
        match self.peek() {
            None => Value::Atom(String::new()),
            Some('"') => self.string_literal(),
            Some('\'') => self.char_literal(),
            Some('(') => match self.items(')') {
                Some(items) => Value::Tuple { name: None, items },
                None => self.degrade(start),
            },
            Some('[') => match self.items(']') {
                Some(items) => Value::List(items),
                None => self.degrade(start),
            },
            Some(next) if next.is_alphabetic() || next == '_' => self.named(start),
            _ => self.degrade(start),
        }
    }

    /// A value opening with an identifier: a struct, a tuple struct or
    /// variant, or a bare atom (unit variant, boolean, or word).
    fn named(&mut self, start: usize) -> Value {
        let name = self.ident();
        let after_ident = self.pos;
        self.skip_whitespace();
        match self.peek() {
            Some('{') => match self.struct_fields() {
                Some(fields) if fields.is_empty() => Value::Atom(name),
                Some(fields) => Value::Struct { name, fields },
                None => self.degrade(start),
            },
            Some('(') => match self.items(')') {
                Some(items) => Value::Tuple {
                    name: Some(name),
                    items,
                },
                None => self.degrade(start),
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
    /// marker (`finish_non_exhaustive`). Returns `None` when the braced
    /// region does not follow the field grammar; the caller degrades it.
    fn struct_fields(&mut self) -> Option<Vec<(String, Value)>> {
        self.bump(); // '{'
        let mut fields = Vec::new();
        loop {
            self.skip_whitespace();
            if self.eat('}') {
                return Some(fields);
            }
            if self.rest().starts_with("..") {
                self.pos += 2;
                self.skip_whitespace();
                return self.eat('}').then_some(fields);
            }
            let name = self.ident();
            if name.is_empty() {
                return None;
            }
            self.skip_whitespace();
            if !self.eat(':') {
                return None;
            }
            fields.push((name, self.element()));
            self.skip_whitespace();
            if self.eat(',') {
                continue;
            }
            if self.eat('}') {
                return Some(fields);
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
            items.push(self.element());
            self.skip_whitespace();
            if self.eat(',') {
                continue;
            }
            if self.eat(closer) {
                return Some(items);
            }
            return None;
        }
    }

    /// One element of a field, tuple, or list: a value that must end at a
    /// separator or closer. When it does not — a custom `Debug` impl printed
    /// something the grammar does not cover — the whole element degrades to
    /// one atom, leaving the enclosing structure parseable.
    fn element(&mut self) -> Value {
        self.skip_whitespace();
        let start = self.pos;
        let value = self.value();
        self.skip_whitespace();
        match self.peek() {
            None | Some(',' | ')' | ']' | '}') => value,
            _ => self.degrade(start),
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
    fn degrade(&mut self, start: usize) -> Value {
        self.pos = start;
        self.consume_balanced_element();
        Value::Atom(self.text[start..self.pos].trim_end().to_string())
    }

    /// Consumes text until, at nesting depth zero, the next character is a
    /// separator (`,`) or a closer belonging to an enclosing region — or the
    /// input ends. String and char literals are skipped escape-aware so
    /// their delimiters and commas cannot unbalance the scan.
    fn consume_balanced_element(&mut self) {
        let mut depth = 0usize;
        while let Some(next) = self.peek() {
            match next {
                '"' => {
                    self.bump();
                    self.literal_body('"');
                }
                '\'' => {
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
                ',' if depth == 0 => return,
                _ => self.bump(),
            }
        }
    }
}

#[cfg(test)]
#[allow(
    dead_code,
    reason = "grammar fixtures are read only through their Debug derives"
)]
mod tests {
    use std::collections::BTreeMap;
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
                fields: vec![
                    field("label", Value::Str("start".to_string())),
                    field(
                        "origin",
                        Value::Struct {
                            name: "Position".to_string(),
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
                fields: vec![
                    field("label", Value::Str("a { b, \\\" c".to_string())),
                    field(
                        "origin",
                        Value::Struct {
                            name: "Position".to_string(),
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
    fn map_debug_output_degrades_to_one_atomic_leaf() {
        let map = BTreeMap::from([(1, "one"), (2, "two")]);

        assert_eq!(parse(&format!("{map:?}")), atom("{1: \"one\", 2: \"two\"}"));
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
                fields: vec![
                    field("tag", atom("PhantomData<u32>")),
                    field("count", atom("4")),
                ],
            }
        );
    }
}
