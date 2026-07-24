use std::{error::Error, fmt, str};

use signalbox_domain::{
    ImportedJsonNumber, ImportedStructuredObjectMember, ImportedStructuredValue, ImportedText,
};

pub(super) const MAX_CONTAINER_DEPTH: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JsonFailure {
    InvalidUtf8,
    Syntax,
    DepthExceeded,
}

impl fmt::Display for JsonFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8 => formatter.write_str("JSON record is not valid UTF-8"),
            Self::Syntax => formatter.write_str("JSON record has invalid syntax"),
            Self::DepthExceeded => formatter.write_str("JSON record exceeds the nesting limit"),
        }
    }
}

impl Error for JsonFailure {}

pub(super) fn parse_record(source: &[u8]) -> Result<ImportedStructuredValue, JsonFailure> {
    let source = str::from_utf8(source).map_err(|_| JsonFailure::InvalidUtf8)?;
    let mut parser = Parser {
        source,
        bytes: source.as_bytes(),
        index: 0,
    };
    let value = parser.parse_value(0)?;
    parser.skip_whitespace();
    if parser.index != parser.bytes.len() {
        return Err(JsonFailure::Syntax);
    }
    Ok(value)
}

struct Parser<'source> {
    source: &'source str,
    bytes: &'source [u8],
    index: usize,
}

impl Parser<'_> {
    fn parse_value(&mut self, depth: usize) -> Result<ImportedStructuredValue, JsonFailure> {
        self.skip_whitespace();
        match self.bytes.get(self.index) {
            Some(b'n') => {
                self.consume_literal(b"null")?;
                Ok(ImportedStructuredValue::Null)
            }
            Some(b't') => {
                self.consume_literal(b"true")?;
                Ok(ImportedStructuredValue::Boolean(true))
            }
            Some(b'f') => {
                self.consume_literal(b"false")?;
                Ok(ImportedStructuredValue::Boolean(false))
            }
            Some(b'"') => self.parse_string().map(ImportedStructuredValue::String),
            Some(b'[') => self.parse_array(depth),
            Some(b'{') => self.parse_object(depth),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            _ => Err(JsonFailure::Syntax),
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<ImportedStructuredValue, JsonFailure> {
        self.enter_container(depth)?;
        self.index += 1;
        self.skip_whitespace();
        let mut values = Vec::new();
        if self.consume_if(b']') {
            return Ok(ImportedStructuredValue::Array(values.into_boxed_slice()));
        }
        loop {
            values.push(self.parse_value(depth + 1)?);
            self.skip_whitespace();
            if self.consume_if(b']') {
                break;
            }
            if !self.consume_if(b',') {
                return Err(JsonFailure::Syntax);
            }
        }
        Ok(ImportedStructuredValue::Array(values.into_boxed_slice()))
    }

    fn parse_object(&mut self, depth: usize) -> Result<ImportedStructuredValue, JsonFailure> {
        self.enter_container(depth)?;
        self.index += 1;
        self.skip_whitespace();
        let mut members = Vec::new();
        if self.consume_if(b'}') {
            return Ok(ImportedStructuredValue::Object(members.into_boxed_slice()));
        }
        loop {
            self.skip_whitespace();
            if self.bytes.get(self.index) != Some(&b'"') {
                return Err(JsonFailure::Syntax);
            }
            let name = self.parse_string()?;
            self.skip_whitespace();
            if !self.consume_if(b':') {
                return Err(JsonFailure::Syntax);
            }
            let value = self.parse_value(depth + 1)?;
            members.push(ImportedStructuredObjectMember::new(name, value));
            self.skip_whitespace();
            if self.consume_if(b'}') {
                break;
            }
            if !self.consume_if(b',') {
                return Err(JsonFailure::Syntax);
            }
        }
        Ok(ImportedStructuredValue::Object(members.into_boxed_slice()))
    }

    fn enter_container(&self, depth: usize) -> Result<(), JsonFailure> {
        if depth >= MAX_CONTAINER_DEPTH {
            Err(JsonFailure::DepthExceeded)
        } else {
            Ok(())
        }
    }

    fn parse_string(&mut self) -> Result<ImportedText, JsonFailure> {
        let start = self.index;
        self.index += 1;
        loop {
            match self.bytes.get(self.index).copied() {
                Some(b'"') => {
                    self.index += 1;
                    let token = self
                        .source
                        .get(start..self.index)
                        .ok_or(JsonFailure::Syntax)?;
                    let decoded =
                        serde_json::from_str::<String>(token).map_err(|_| JsonFailure::Syntax)?;
                    return Ok(ImportedText::new(decoded));
                }
                Some(b'\\') => {
                    self.index += 1;
                    if self.bytes.get(self.index).is_none() {
                        return Err(JsonFailure::Syntax);
                    }
                    self.index += 1;
                }
                Some(0x00..=0x1f) | None => return Err(JsonFailure::Syntax),
                Some(_) => self.index += 1,
            }
        }
    }

    fn parse_number(&mut self) -> Result<ImportedStructuredValue, JsonFailure> {
        let start = self.index;
        if self.consume_if(b'-') && self.bytes.get(self.index).is_none() {
            return Err(JsonFailure::Syntax);
        }
        match self.bytes.get(self.index) {
            Some(b'0') => self.index += 1,
            Some(b'1'..=b'9') => {
                self.index += 1;
                self.consume_digits();
            }
            _ => return Err(JsonFailure::Syntax),
        }
        if self.consume_if(b'.') {
            let digits = self.index;
            self.consume_digits();
            if self.index == digits {
                return Err(JsonFailure::Syntax);
            }
        }
        if matches!(self.bytes.get(self.index), Some(b'e' | b'E')) {
            self.index += 1;
            if matches!(self.bytes.get(self.index), Some(b'+' | b'-')) {
                self.index += 1;
            }
            let digits = self.index;
            self.consume_digits();
            if self.index == digits {
                return Err(JsonFailure::Syntax);
            }
        }
        let spelling = self
            .source
            .get(start..self.index)
            .ok_or(JsonFailure::Syntax)?;
        let number =
            ImportedJsonNumber::try_new(String::from(spelling)).map_err(|_| JsonFailure::Syntax)?;
        Ok(ImportedStructuredValue::Number(number))
    }

    fn consume_digits(&mut self) {
        while matches!(self.bytes.get(self.index), Some(b'0'..=b'9')) {
            self.index += 1;
        }
    }

    fn consume_literal(&mut self, literal: &[u8]) -> Result<(), JsonFailure> {
        let end = self
            .index
            .checked_add(literal.len())
            .ok_or(JsonFailure::Syntax)?;
        if self.bytes.get(self.index..end) != Some(literal) {
            return Err(JsonFailure::Syntax);
        }
        self.index = end;
        Ok(())
    }

    fn consume_if(&mut self, expected: u8) -> bool {
        if self.bytes.get(self.index) == Some(&expected) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(
            self.bytes.get(self.index),
            Some(b' ' | b'\t' | b'\r' | b'\n')
        ) {
            self.index += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use signalbox_domain::{ImportedStructuredValue, ImportedText};

    use super::{JsonFailure, parse_record};

    #[test]
    fn preserves_object_order_duplicates_and_number_spelling() {
        let parsed = parse_record(br#"{"same":1e+09,"same":-0.25,"text":"\u0000"}"#)
            .expect("synthetic JSON is valid");
        let ImportedStructuredValue::Object(members) = parsed else {
            panic!("synthetic root should be an object");
        };
        assert_eq!(members.len(), 3);
        assert_eq!(members[0].name().as_str(), "same");
        assert_eq!(members[1].name().as_str(), "same");
        let ImportedStructuredValue::Number(first) = members[0].value() else {
            panic!("first synthetic duplicate should be a number");
        };
        assert_eq!(first.as_str(), "1e+09");
        let ImportedStructuredValue::String(value) = members[2].value() else {
            panic!("synthetic text should be a string");
        };
        assert_eq!(value, &ImportedText::new(String::from("\0")));
    }

    #[test]
    fn bounds_container_nesting() {
        let accepted = format!("{}0{}", "[".repeat(128), "]".repeat(128));
        let rejected = format!("{}0{}", "[".repeat(129), "]".repeat(129));
        assert!(parse_record(accepted.as_bytes()).is_ok());
        assert_eq!(
            parse_record(rejected.as_bytes()),
            Err(JsonFailure::DepthExceeded)
        );
    }
}
