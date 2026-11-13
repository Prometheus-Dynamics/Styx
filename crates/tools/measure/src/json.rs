//! A small JSON writer: enough for the report (numbers, strings, null, arrays, objects). Written
//! by hand so the tool needs no serialiser at run time; the tests parse its output with
//! `serde_json`.

use std::fmt::Write;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(u64),
    /// A float; NaN and infinities are written as `null`.
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(&'static str, Json)>),
}

impl Json {
    pub fn opt_int(v: Option<u64>) -> Json {
        v.map_or(Json::Null, Json::Int)
    }

    pub fn opt_float(v: Option<f64>) -> Json {
        v.map_or(Json::Null, Json::Float)
    }

    pub fn opt_str(v: Option<&str>) -> Json {
        v.map_or(Json::Null, |s| Json::Str(s.to_string()))
    }

    /// The value as one string of JSON text.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(v) => {
                let _ = write!(out, "{v}");
            }
            Json::Float(v) if v.is_finite() => {
                let _ = write!(out, "{v}");
            }
            Json::Float(_) => out.push_str("null"),
            Json::Str(s) => write_str(out, s),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(fields) => {
                out.push('{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_str(out, key);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_str(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_and_nests() {
        let v = Json::Object(vec![
            ("a", Json::Int(3)),
            ("b", Json::Float(f64::NAN)),
            ("c", Json::Str("x\"y\\z\n\u{1}".into())),
            ("d", Json::Array(vec![Json::Null, Json::Bool(true)])),
        ]);
        assert_eq!(
            v.render(),
            "{\"a\":3,\"b\":null,\"c\":\"x\\\"y\\\\z\\n\\u0001\",\"d\":[null,true]}"
        );
    }
}
