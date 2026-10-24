//! Minimal JSON value model, parser and serializer — hand-rolled so
//! SettingsKit stays zero-dependency (house rule). Objects preserve
//! insertion order: a settings file the user (or another tool) wrote
//! keeps its key order across a load/save round-trip.
//!
//! Numbers: an integral literal that fits i64 is stored as `I64`;
//! anything else is `F64`. Only finite floats are representable —
//! `F64` values are validated on construction at the API surface.

use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(String),
    Array(Vec<Value>),
    /// Insertion-ordered key/value pairs. Keys are unique — the
    /// parser rejects duplicates within one object (a settings file
    /// with two values for one key is ambiguous, and silently
    /// picking one is how settings bugs hide).
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        match self {
            Value::Object(pairs) => pairs.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Set `key` on an object value, preserving first-insertion
    /// order. Returns false if this value is not an object.
    pub fn set(&mut self, key: &str, value: Value) -> bool {
        match self {
            Value::Object(pairs) => {
                if let Some(slot) = pairs.iter_mut().find(|(k, _)| k == key) {
                    slot.1 = value;
                } else {
                    pairs.push((key.to_string(), value));
                }
                true
            }
            _ => false,
        }
    }

    /// Remove `key` from an object value. Returns the removed value.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        match self {
            Value::Object(pairs) => {
                let idx = pairs.iter().position(|(k, _)| k == key)?;
                Some(pairs.remove(idx).1)
            }
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::I64(n) => Some(*n),
            Value::F64(f) if f.fract() == 0.0 && f.is_finite() => Some(*f as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::I64(n) => Some(*n as f64),
            Value::F64(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::I64(_) | Value::F64(_) => "number",
            Value::Str(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }
}

/// Deepest nesting the parser accepts. Settings files are shallow;
/// the cap exists so a hostile or corrupt file cannot overflow the
/// stack through the recursive parser (or the serializer).
pub const MAX_DEPTH: usize = 128;

pub fn parse(text: &str) -> Result<Value, String> {
    let mut p = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let v = p.value(0)?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(format!("trailing characters at byte {}", p.pos));
    }
    Ok(v)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", b as char, self.pos))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err(format!("nesting deeper than {MAX_DEPTH} levels"));
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(format!("unexpected character at byte {}", self.pos)),
        }
    }

    fn literal(&mut self, word: &str, v: Value) -> Result<Value, String> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(format!("invalid literal at byte {}", self.pos))
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, String> {
        self.expect(b'{')?;
        let mut pairs: Vec<(String, Value)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            if pairs.iter().any(|(k, _)| k == &key) {
                return Err(format!("duplicate key \"{key}\" in object"));
            }
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let val = self.value(depth + 1)?;
            pairs.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(pairs));
                }
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.pos)),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(format!("expected ',' or ']' at byte {}", self.pos)),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let b = self.peek().ok_or("unterminated string")?;
            self.pos += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let esc = self.peek().ok_or("unterminated escape")?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            if (0xD800..0xDC00).contains(&hi) {
                                // High surrogate: a \uXXXX low must follow.
                                if self.peek() == Some(b'\\') {
                                    self.pos += 1;
                                    self.expect(b'u')?;
                                    let lo = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&lo) {
                                        return Err("invalid low surrogate".into());
                                    }
                                    let c = 0x1_0000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                    out.push(char::from_u32(c).ok_or("invalid unicode escape")?);
                                } else {
                                    return Err("unpaired high surrogate".into());
                                }
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return Err("unpaired low surrogate".into());
                            } else {
                                out.push(char::from_u32(hi).ok_or("invalid unicode escape")?);
                            }
                        }
                        _ => return Err(format!("invalid escape at byte {}", self.pos - 1)),
                    }
                }
                0x00..=0x1F => {
                    return Err(format!(
                        "unescaped control character at byte {}",
                        self.pos - 1
                    ))
                }
                _ => {
                    // Consume one full UTF-8 char (input is &str, so
                    // the bytes are valid UTF-8; copy the sequence).
                    let start = self.pos - 1;
                    let mut end = self.pos;
                    while end < self.bytes.len() && (self.bytes[end] & 0xC0) == 0x80 {
                        end += 1;
                    }
                    out.push_str(
                        std::str::from_utf8(&self.bytes[start..end])
                            .map_err(|_| "invalid UTF-8 in string".to_string())?,
                    );
                    self.pos = end;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut v: u32 = 0;
        for _ in 0..4 {
            let b = self.peek().ok_or("truncated \\u escape")?;
            self.pos += 1;
            let digit = (b as char)
                .to_digit(16)
                .ok_or("invalid hex in \\u escape")?;
            v = v * 16 + digit;
        }
        Ok(v)
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // Integer part: "0" or [1-9][0-9]* — leading zeros are invalid JSON.
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(format!("invalid number at byte {start}")),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(format!("invalid fraction at byte {}", self.pos));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(format!("invalid exponent at byte {}", self.pos));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap();
        if !is_float {
            if let Ok(n) = text.parse::<i64>() {
                return Ok(Value::I64(n));
            }
        }
        let f: f64 = text
            .parse()
            .map_err(|_| format!("invalid number '{text}'"))?;
        if !f.is_finite() {
            return Err(format!("number out of range '{text}'"));
        }
        Ok(Value::F64(f))
    }
}

/// Serialize with two-space indentation — the shape every app in
/// the family writes its settings in. A trailing newline is added
/// by the caller when writing files, not here.
pub fn to_string_pretty(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v, 0);
    out
}

/// Compact serialization (single line) — used for `sk_get_json`.
pub fn to_string_compact(v: &Value) -> String {
    let mut out = String::new();
    write_compact(&mut out, v);
    out
}

fn write_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn write_value(out: &mut String, v: &Value, depth: usize) {
    match v {
        Value::Object(pairs) => {
            if pairs.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (i, (k, val)) in pairs.iter().enumerate() {
                write_indent(out, depth + 1);
                write_json_string(out, k);
                out.push_str(": ");
                write_value(out, val, depth + 1);
                if i + 1 < pairs.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            write_indent(out, depth);
            out.push('}');
        }
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                write_indent(out, depth + 1);
                write_value(out, item, depth + 1);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            write_indent(out, depth);
            out.push(']');
        }
        _ => write_scalar(out, v),
    }
}

fn write_compact(out: &mut String, v: &Value) {
    match v {
        Value::Object(pairs) => {
            out.push('{');
            for (i, (k, val)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(out, k);
                out.push(':');
                write_compact(out, val);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_compact(out, item);
            }
            out.push(']');
        }
        _ => write_scalar(out, v),
    }
}

fn write_scalar(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::I64(n) => {
            let _ = write!(out, "{n}");
        }
        Value::F64(f) => write_float(out, *f),
        Value::Str(s) => write_json_string(out, s),
        Value::Array(_) | Value::Object(_) => unreachable!("handled by callers"),
    }
}

fn write_float(out: &mut String, f: f64) {
    // Rust's Display for f64 never uses exponents and always emits
    // a fraction for integral values via format below; JSON accepts
    // both "2" and "2.0" for a float — emit the shortest form that
    // round-trips, adding ".0" when the value reads as an integer
    // so a re-parse keeps it a float for consumers that care.
    if f == f.trunc() && f.abs() < 1e15 {
        let _ = write!(out, "{f:.1}");
    } else {
        let _ = write!(out, "{f}");
    }
}

fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    out.push_str(&crate::json_escape(s));
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(text: &str) -> Value {
        let v = parse(text).unwrap();
        let pretty = to_string_pretty(&v);
        parse(&pretty).unwrap()
    }

    #[test]
    fn parses_scalars() {
        assert_eq!(parse("null").unwrap(), Value::Null);
        assert_eq!(parse("true").unwrap(), Value::Bool(true));
        assert_eq!(parse("-42").unwrap(), Value::I64(-42));
        assert_eq!(parse("2.5").unwrap(), Value::F64(2.5));
        assert_eq!(parse("1e3").unwrap(), Value::F64(1000.0));
        assert_eq!(parse("\"a\\n\\u0041\"").unwrap(), Value::Str("a\nA".into()));
    }

    #[test]
    fn parses_surrogate_pairs() {
        assert_eq!(
            parse("\"\\uD83D\\uDE00\"").unwrap(),
            Value::Str("😀".into())
        );
        assert!(parse("\"\\uD83D\"").is_err());
        assert!(parse("\"\\uDE00\"").is_err());
    }

    #[test]
    fn roundtrips_nested() {
        let v = roundtrip(r#"{"a": [1, 2.5, {"b": null}], "c": {"d": []}}"#);
        assert_eq!(v.get("c").unwrap().get("d").unwrap(), &Value::Array(vec![]));
    }

    #[test]
    fn preserves_key_order() {
        let v = parse(r#"{"z": 1, "a": 2, "m": 3}"#).unwrap();
        let out = to_string_compact(&v);
        assert_eq!(out, r#"{"z":1,"a":2,"m":3}"#);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("").is_err());
        assert!(parse("{} extra").is_err());
        assert!(parse("{\"a\": }").is_err());
        assert!(parse("{\"a\": 1, \"a\": 2}").is_err()); // duplicate key
        assert!(parse("[1,]").is_err());
        assert!(parse("01").is_err());
        assert!(parse("\"unterminated").is_err());
        assert!(parse("\"bad \\x escape\"").is_err());
        assert!(parse("1e999").is_err()); // out of f64 range
    }

    #[test]
    fn depth_cap() {
        let deep = "[".repeat(500) + &"]".repeat(500);
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn float_formatting_roundtrips() {
        for f in [2.0f64, 0.5, -3.25, 100.0, 0.1] {
            let v = Value::F64(f);
            let text = to_string_compact(&v);
            assert_eq!(parse(&text).unwrap().as_f64().unwrap(), f, "text={text}");
        }
    }

    #[test]
    fn object_mutation() {
        let mut v = parse(r#"{"a": 1}"#).unwrap();
        assert!(v.set("b", Value::Bool(true)));
        assert_eq!(v.get("b"), Some(&Value::Bool(true)));
        assert_eq!(v.remove("a"), Some(Value::I64(1)));
        assert_eq!(v.remove("a"), None);
    }
}
