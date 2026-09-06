//! Lossless imported structured values for `docs/spec/conversation-import.md`.

use std::error::Error;
use std::fmt;
use std::hash::Hash;
use std::hash::Hasher;
use std::str::FromStr;

/// What an external source asserted about one field.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ImportedSourceAttestation<Value> {
    /// The source supplied this exact value.
    Attested(Value),
    /// The source supplied an explicit null value.
    AttestedAbsent,
    /// The source did not supply the field.
    NotAttested,
}

/// Exact decoded imported text, including empty text and U+0000.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ImportedText(String);

impl ImportedText {
    /// Preserves one decoded Unicode scalar sequence without rewriting it.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Borrows the exact decoded text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact decoded text.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for ImportedText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportedText")
            .field("utf8_len", &self.0.len())
            .finish_non_exhaustive()
    }
}

/// One checked JSON number spelling in the source-neutral structured algebra.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ImportedJsonNumber(String);

impl ImportedJsonNumber {
    /// Checks the complete RFC 8259 JSON number grammar.
    pub fn try_new(value: String) -> Result<Self, ImportedJsonNumberError> {
        if serde_json::Number::from_str(&value).is_ok() {
            Ok(Self(value))
        } else {
            Err(ImportedJsonNumberError { value })
        }
    }

    /// Borrows the checked number spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the checked number spelling.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for ImportedJsonNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportedJsonNumber")
            .field("utf8_len", &self.0.len())
            .finish_non_exhaustive()
    }
}

/// A rejected imported JSON number retaining its exact spelling.
#[derive(Clone, Eq, PartialEq)]
pub struct ImportedJsonNumberError {
    value: String,
}

impl ImportedJsonNumberError {
    /// Borrows the rejected number spelling.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the rejected number spelling.
    pub fn into_value(self) -> String {
        self.value
    }
}

impl fmt::Debug for ImportedJsonNumberError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportedJsonNumberError")
            .field("utf8_len", &self.value.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Display for ImportedJsonNumberError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("imported JSON number has invalid syntax")
    }
}

impl Error for ImportedJsonNumberError {}

/// One ordered object member in the source-neutral structured algebra.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ImportedStructuredObjectMember {
    name: ImportedText,
    value: ImportedStructuredValue,
}

impl ImportedStructuredObjectMember {
    /// Preserves one object member and its physical member position.
    pub fn new(name: ImportedText, value: ImportedStructuredValue) -> Self {
        Self { name, value }
    }

    /// Borrows the exact decoded member name.
    pub const fn name(&self) -> &ImportedText {
        &self.name
    }

    /// Borrows the member value.
    pub const fn value(&self) -> &ImportedStructuredValue {
        &self.value
    }
}

/// Source-neutral decoded JSON values.
pub enum ImportedStructuredValue {
    /// JSON null.
    Null,
    /// JSON boolean.
    Boolean(bool),
    /// Checked JSON number.
    Number(ImportedJsonNumber),
    /// Exact decoded JSON string.
    String(ImportedText),
    /// Ordered JSON array.
    Array(Box<[ImportedStructuredValue]>),
    /// Ordered JSON object members, including repeated names.
    Object(Box<[ImportedStructuredObjectMember]>),
}

impl Clone for ImportedStructuredValue {
    fn clone(&self) -> Self {
        enum Task<'a> {
            Visit(&'a ImportedStructuredValue),
            FinishArray(usize),
            FinishObject(&'a [ImportedStructuredObjectMember]),
        }

        let mut tasks = vec![Task::Visit(self)];
        let mut built = Vec::new();
        while let Some(task) = tasks.pop() {
            match task {
                Task::Visit(Self::Null) => built.push(Self::Null),
                Task::Visit(Self::Boolean(value)) => built.push(Self::Boolean(*value)),
                Task::Visit(Self::Number(value)) => built.push(Self::Number(value.clone())),
                Task::Visit(Self::String(value)) => built.push(Self::String(value.clone())),
                Task::Visit(Self::Array(values)) => {
                    tasks.push(Task::FinishArray(values.len()));
                    tasks.extend(values.iter().rev().map(Task::Visit));
                }
                Task::Visit(Self::Object(members)) => {
                    tasks.push(Task::FinishObject(members));
                    tasks.extend(
                        members
                            .iter()
                            .rev()
                            .map(|member| Task::Visit(member.value())),
                    );
                }
                Task::FinishArray(value_count) => {
                    let start = built.len().saturating_sub(value_count);
                    let values = built.split_off(start).into_boxed_slice();
                    built.push(Self::Array(values));
                }
                Task::FinishObject(members) => {
                    let start = built.len().saturating_sub(members.len());
                    let values = built.split_off(start);
                    let cloned_members = members
                        .iter()
                        .zip(values)
                        .map(|(member, value)| {
                            ImportedStructuredObjectMember::new(member.name().clone(), value)
                        })
                        .collect::<Vec<_>>()
                        .into_boxed_slice();
                    built.push(Self::Object(cloned_members));
                }
            }
        }

        debug_assert_eq!(built.len(), 1);
        built.pop().unwrap_or(Self::Null)
    }
}

impl fmt::Debug for ImportedStructuredValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        enum Part<'a> {
            Value(&'a ImportedStructuredValue),
            Member(&'a ImportedStructuredObjectMember),
            Literal(&'static str),
        }

        let mut pending = vec![Part::Value(self)];
        while let Some(part) = pending.pop() {
            match part {
                Part::Value(Self::Null) => formatter.write_str("Null")?,
                Part::Value(Self::Boolean(value)) => {
                    write!(formatter, "Boolean({value:?})")?;
                }
                Part::Value(Self::Number(value)) => {
                    formatter.write_str("Number(")?;
                    fmt::Debug::fmt(value, formatter)?;
                    formatter.write_str(")")?;
                }
                Part::Value(Self::String(value)) => {
                    formatter.write_str("String(")?;
                    fmt::Debug::fmt(value, formatter)?;
                    formatter.write_str(")")?;
                }
                Part::Value(Self::Array(values)) => {
                    formatter.write_str("Array([")?;
                    pending.push(Part::Literal("])"));
                    for (index, value) in values.iter().enumerate().rev() {
                        pending.push(Part::Value(value));
                        if index != 0 {
                            pending.push(Part::Literal(", "));
                        }
                    }
                }
                Part::Value(Self::Object(members)) => {
                    formatter.write_str("Object([")?;
                    pending.push(Part::Literal("])"));
                    for (index, member) in members.iter().enumerate().rev() {
                        pending.push(Part::Member(member));
                        if index != 0 {
                            pending.push(Part::Literal(", "));
                        }
                    }
                }
                Part::Member(member) => {
                    formatter.write_str("ImportedStructuredObjectMember { name: ")?;
                    fmt::Debug::fmt(member.name(), formatter)?;
                    formatter.write_str(", value: ")?;
                    pending.push(Part::Literal(" }"));
                    pending.push(Part::Value(member.value()));
                }
                Part::Literal(value) => formatter.write_str(value)?,
            }
        }
        Ok(())
    }
}

impl PartialEq for ImportedStructuredValue {
    fn eq(&self, other: &Self) -> bool {
        let mut pending = vec![(self, other)];
        while let Some((left, right)) = pending.pop() {
            match (left, right) {
                (Self::Null, Self::Null) => {}
                (Self::Boolean(left), Self::Boolean(right)) if left == right => {}
                (Self::Number(left), Self::Number(right)) if left == right => {}
                (Self::String(left), Self::String(right)) if left == right => {}
                (Self::Array(left), Self::Array(right)) if left.len() == right.len() => {
                    pending.extend(left.iter().zip(right.iter()));
                }
                (Self::Object(left), Self::Object(right)) if left.len() == right.len() => {
                    for (left, right) in left.iter().zip(right.iter()) {
                        if left.name() != right.name() {
                            return false;
                        }
                        pending.push((left.value(), right.value()));
                    }
                }
                _ => return false,
            }
        }
        true
    }
}

impl Eq for ImportedStructuredValue {}

impl Hash for ImportedStructuredValue {
    fn hash<State>(&self, state: &mut State)
    where
        State: Hasher,
    {
        enum Part<'a> {
            Value(&'a ImportedStructuredValue),
            MemberName(&'a ImportedText),
        }

        let mut pending = vec![Part::Value(self)];
        while let Some(part) = pending.pop() {
            match part {
                Part::Value(value) => {
                    std::mem::discriminant(value).hash(state);
                    match value {
                        Self::Null => {}
                        Self::Boolean(value) => value.hash(state),
                        Self::Number(value) => value.hash(state),
                        Self::String(value) => value.hash(state),
                        Self::Array(values) => {
                            values.len().hash(state);
                            pending.extend(values.iter().rev().map(Part::Value));
                        }
                        Self::Object(members) => {
                            members.len().hash(state);
                            for member in members.iter().rev() {
                                pending.push(Part::Value(member.value()));
                                pending.push(Part::MemberName(member.name()));
                            }
                        }
                    }
                }
                Part::MemberName(name) => name.hash(state),
            }
        }
    }
}

impl Drop for ImportedStructuredValue {
    fn drop(&mut self) {
        let mut pending = Vec::new();
        take_structured_children(self, &mut pending);
        while let Some(mut value) = pending.pop() {
            if matches!(
                &value,
                ImportedStructuredValue::Array(_) | ImportedStructuredValue::Object(_)
            ) {
                take_structured_children(&mut value, &mut pending);
                // The container now owns only the empty boxed slice installed by
                // `take_structured_children`; forgetting it prevents re-entering
                // this destructor once per nesting level.
                std::mem::forget(value);
            }
        }
    }
}

fn take_structured_children(
    value: &mut ImportedStructuredValue,
    pending: &mut Vec<ImportedStructuredValue>,
) {
    match value {
        ImportedStructuredValue::Array(values) => {
            pending.extend(std::mem::take(values).into_vec());
        }
        ImportedStructuredValue::Object(members) => {
            pending.extend(
                std::mem::take(members)
                    .into_vec()
                    .into_iter()
                    .map(|member| member.value),
            );
        }
        ImportedStructuredValue::Null
        | ImportedStructuredValue::Boolean(_)
        | ImportedStructuredValue::Number(_)
        | ImportedStructuredValue::String(_) => {}
    }
}
