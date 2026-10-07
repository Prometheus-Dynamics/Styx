//! Just enough JSON writing for the settings file (no serde: the recorder stays small), and
//! UTC timestamps.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

/// A JSON string literal.
pub fn string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
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
    out
}

/// A JSON number (`null` when not finite).
pub fn number(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "null".into()
    }
}

/// `v` as JSON, or `null`.
pub fn opt<T>(v: Option<T>, f: impl FnOnce(T) -> String) -> String {
    v.map_or_else(|| "null".into(), f)
}

/// An object written field by field, pretty-printed at `indent`.
pub struct Object {
    out: String,
    indent: usize,
    first: bool,
}

impl Object {
    pub fn new(indent: usize) -> Self {
        Self {
            out: "{".into(),
            indent,
            first: true,
        }
    }

    /// A field whose value is already JSON.
    pub fn raw(&mut self, key: &str, value: impl AsRef<str>) -> &mut Self {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
        let pad = " ".repeat(self.indent + 2);
        let _ = write!(self.out, "\n{pad}{}: {}", string(key), value.as_ref());
        self
    }

    pub fn str(&mut self, key: &str, value: &str) -> &mut Self {
        self.raw(key, string(value))
    }

    pub fn finish(mut self) -> String {
        if !self.first {
            self.out.push('\n');
            self.out.push_str(&" ".repeat(self.indent));
        }
        self.out.push('}');
        self.out
    }
}

/// A JSON array of already-JSON items, one per line at `indent`.
pub fn array(items: &[String], indent: usize) -> String {
    if items.is_empty() {
        return "[]".into();
    }
    let pad = " ".repeat(indent + 2);
    let body: Vec<String> = items.iter().map(|i| format!("{pad}{i}")).collect();
    format!("[\n{}\n{}]", body.join(",\n"), " ".repeat(indent))
}

/// Nanoseconds since the Unix epoch now.
pub fn unix_ns_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// `unix_ns` as an RFC 3339 UTC time with milliseconds (`2026-10-06T12:34:56.789Z`).
pub fn utc(unix_ns: u64) -> String {
    let secs = unix_ns / 1_000_000_000;
    let ms = (unix_ns / 1_000_000) % 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_strings() {
        assert_eq!(string("a\"b\\c\n\u{1}"), r#""a\"b\\c\n\u0001""#);
    }

    #[test]
    fn formats_utc() {
        assert_eq!(utc(0), "1970-01-01T00:00:00.000Z");
        // 2026-10-06T12:34:56.789Z
        assert_eq!(utc(1_791_290_096_789_000_000), "2026-10-06T12:34:56.789Z");
        assert_eq!(utc(951_782_400_000_000_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn writes_objects() {
        let mut o = Object::new(0);
        o.str("a", "x")
            .raw("b", number(1.5))
            .raw("c", opt(None::<u8>, |v| v.to_string()));
        assert_eq!(
            o.finish(),
            "{\n  \"a\": \"x\",\n  \"b\": 1.5,\n  \"c\": null\n}"
        );
        assert_eq!(Object::new(0).finish(), "{}");
    }
}
