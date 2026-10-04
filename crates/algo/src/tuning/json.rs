//! A small, lenient JSON reader for Raspberry Pi tuning files.
//!
//! libcamera reads these files with a YAML parser, so files in the wild contain trailing commas
//! (e.g. `imx283.json`) and rely on object key order (the first mode listed is the default).
//! This reader keeps key order, accepts trailing commas and `#` / `//` line comments, and is
//! otherwise strict JSON.

use alloc::{string::String, vec::Vec};

use crate::error::{AlgoError, Result};

/// A JSON value with ordered objects.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// A number.
    Number(f64),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<Value>),
    /// An object, keys in file order.
    Object(Vec<(String, Value)>),
}

impl Value {
    /// Look up a key of an object.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(o) => o.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The number, if this is one (booleans count as 0/1, as libcamera's reader allows).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            Value::Bool(b) => Some(f64::from(u8::from(*b))),
            _ => None,
        }
    }

    /// The string, if this is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// The array, if this is one.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// The object entries, if this is one.
    pub fn as_object(&self) -> Option<&[(String, Value)]> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }
}

/// Parse a document.
pub fn parse(text: &str) -> Result<Value> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
    };
    let v = p.value(0)?;
    p.ws();
    if p.i != p.s.len() {
        return Err(p.err("trailing characters"));
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn err(&self, m: &str) -> AlgoError {
        AlgoError::Json {
            offset: self.i,
            message: m.into(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn ws(&mut self) {
        while let Some(c) = self.peek() {
            match c {
                b' ' | b'\t' | b'\r' | b'\n' => self.i += 1,
                b'#' => self.skip_line(),
                b'/' if self.s.get(self.i + 1) == Some(&b'/') => self.skip_line(),
                _ => break,
            }
        }
    }

    fn skip_line(&mut self) {
        while let Some(c) = self.peek() {
            self.i += 1;
            if c == b'\n' {
                break;
            }
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > 64 {
            return Err(self.err("nesting too deep"));
        }
        self.ws();
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b't') => self.word("true", Value::Bool(true)),
            Some(b'f') => self.word("false", Value::Bool(false)),
            Some(b'n') => self.word("null", Value::Null),
            Some(c) if c == b'-' || c == b'+' || c == b'.' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(self.err("unexpected character")),
            None => Err(self.err("unexpected end")),
        }
    }

    fn word(&mut self, w: &str, v: Value) -> Result<Value> {
        if self.s[self.i..].starts_with(w.as_bytes()) {
            self.i += w.len();
            Ok(v)
        } else {
            Err(self.err("unknown literal"))
        }
    }

    fn number(&mut self) -> Result<Value> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E') {
                self.i += 1;
            } else {
                break;
            }
        }
        let text = core::str::from_utf8(&self.s[start..self.i]).map_err(|_| self.err("utf-8"))?;
        text.parse::<f64>()
            .map(Value::Number)
            .map_err(|_| self.err("bad number"))
    }

    fn string(&mut self) -> Result<String> {
        self.i += 1; // opening quote
        let mut out = Vec::new();
        loop {
            let c = self.peek().ok_or_else(|| self.err("unterminated string"))?;
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = self.peek().ok_or_else(|| self.err("bad escape"))?;
                    self.i += 1;
                    match e {
                        b'"' | b'\\' | b'/' => out.push(e),
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let hex = self
                                .s
                                .get(self.i..self.i + 4)
                                .and_then(|h| core::str::from_utf8(h).ok())
                                .and_then(|h| u32::from_str_radix(h, 16).ok())
                                .ok_or_else(|| self.err("bad \\u escape"))?;
                            self.i += 4;
                            let ch = char::from_u32(hex).unwrap_or('\u{fffd}');
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return Err(self.err("bad escape")),
                    }
                }
                _ => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| self.err("utf-8"))
    }

    fn array(&mut self, depth: usize) -> Result<Value> {
        self.i += 1;
        let mut items = Vec::new();
        loop {
            self.ws();
            if self.peek() == Some(b']') {
                self.i += 1;
                return Ok(Value::Array(items));
            }
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {}
                _ => return Err(self.err("expected , or ]")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value> {
        self.i += 1;
        let mut items = Vec::new();
        loop {
            self.ws();
            match self.peek() {
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Object(items));
                }
                Some(b'"') => {}
                _ => return Err(self.err("expected key")),
            }
            let key = self.string()?;
            self.ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected :"));
            }
            self.i += 1;
            let v = self.value(depth + 1)?;
            items.push((key, v));
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {}
                _ => return Err(self.err("expected , or }")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_order_and_accepts_trailing_commas() {
        let v = parse(
            r#"{ "b": [1, 2.5e1, -3,], "a": {"x": "q\"A"}, # c
        "t": true, }"#,
        )
        .unwrap();
        let o = v.as_object().unwrap();
        assert_eq!(o[0].0, "b");
        assert_eq!(o[1].0, "a");
        assert_eq!(
            v.get("b").unwrap().as_array().unwrap()[1].as_f64(),
            Some(25.0)
        );
        assert_eq!(v.get("a").unwrap().get("x").unwrap().as_str(), Some("q\"A"));
        assert_eq!(v.get("t").unwrap().as_f64(), Some(1.0));
    }

    #[test]
    fn reports_errors_with_offsets() {
        let e = parse("{\"a\" 1}").unwrap_err();
        assert!(matches!(e, AlgoError::Json { offset: 5, .. }), "{e}");
        assert!(parse("[1, 2").is_err());
        assert!(parse("{} x").is_err());
    }
}
