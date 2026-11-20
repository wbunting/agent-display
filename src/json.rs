//! `json.dumps` as Python prints it: `", "`/`": "` separators, optional
//! `indent`, `ensure_ascii`, and `repr(float)` numbers. Objects keep their
//! insertion order (serde_json `preserve_order`).

use serde_json::{Map, Number, Value};

pub fn dumps(v: &Value, ensure_ascii: bool) -> String {
    let mut out = String::new();
    write(&mut out, v, None, 0, ensure_ascii);
    out
}

pub fn dumps_indent(v: &Value, indent: usize, ensure_ascii: bool) -> String {
    let mut out = String::new();
    write(&mut out, v, Some(indent), 0, ensure_ascii);
    out
}

fn write(out: &mut String, v: &Value, indent: Option<usize>, level: usize, ascii: bool) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number(n)),
        Value::String(s) => string(out, s, ascii),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                sep(out, i, indent, level + 1);
                write(out, item, indent, level + 1, ascii);
            }
            close(out, indent, level);
            out.push(']');
        }
        Value::Object(map) => object(out, map, indent, level, ascii),
    }
}

fn object(out: &mut String, map: &Map<String, Value>, indent: Option<usize>, level: usize, ascii: bool) {
    if map.is_empty() {
        out.push_str("{}");
        return;
    }
    out.push('{');
    for (i, (k, val)) in map.iter().enumerate() {
        sep(out, i, indent, level + 1);
        string(out, k, ascii);
        out.push_str(": ");
        write(out, val, indent, level + 1, ascii);
    }
    close(out, indent, level);
    out.push('}');
}

fn sep(out: &mut String, i: usize, indent: Option<usize>, level: usize) {
    match indent {
        None => {
            if i > 0 {
                out.push_str(", ");
            }
        }
        Some(n) => {
            if i > 0 {
                out.push(',');
            }
            out.push('\n');
            out.push_str(&" ".repeat(n * level));
        }
    }
}

fn close(out: &mut String, indent: Option<usize>, level: usize) {
    if let Some(n) = indent {
        out.push('\n');
        out.push_str(&" ".repeat(n * level));
    }
}

fn string(out: &mut String, s: &str, ascii: bool) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ascii && (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn number(n: &Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    float_repr(n.as_f64().unwrap_or(f64::NAN))
}

/// Python's `float.__repr__`: shortest round-trip digits, fixed notation
/// for exponents in [-4, 16), else `d.ddde+XX`.
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // `{:e}` is the shortest round-trip representation in scientific form.
    let sci = format!("{x:e}");
    let (mant, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let sign = if neg { "-" } else { "" };
    if (-4..16).contains(&exp) {
        let point = exp + 1; // digits before the decimal point
        let s = if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if point as usize >= digits.len() {
            format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
        } else {
            format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
        };
        format!("{sign}{s}")
    } else {
        let m = if digits.len() > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits };
        format!("{sign}{m}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compact_matches_python() {
        let v: Value = serde_json::from_str(r#"{"tab": "X", "port": 9425, "output": null}"#).unwrap();
        assert_eq!(dumps(&v, true), r#"{"tab": "X", "port": 9425, "output": null}"#);
        assert_eq!(dumps(&json!("日本\u{1F642}"), true), r#""\u65e5\u672c\ud83d\ude42""#);
        assert_eq!(dumps(&json!("日本"), false), "\"日本\"");
        assert_eq!(dumps(&json!("a\"\\\n\u{1}\u{7f}"), true), r#""a\"\\\n\u0001\u007f""#);
        assert_eq!(dumps(&json!("\u{7f}"), false), "\"\u{7f}\"");
        assert_eq!(dumps(&json!([]), true), "[]");
        assert_eq!(dumps(&json!({}), true), "{}");
    }

    #[test]
    fn indent_matches_python() {
        let v = json!({"port": 9425, "tabs": [{"tab": "A"}], "e": [], "o": {}});
        assert_eq!(
            dumps_indent(&v, 2, false),
            "{\n  \"port\": 9425,\n  \"tabs\": [\n    {\n      \"tab\": \"A\"\n    }\n  ],\n  \"e\": [],\n  \"o\": {}\n}"
        );
    }

    #[test]
    fn floats_match_python_repr() {
        for (x, s) in [(1.0, "1.0"), (0.1 + 0.2, "0.30000000000000004"), (1e16, "1e+16"),
                       (1e15, "1000000000000000.0"), (0.0001, "0.0001"), (0.00001, "1e-05"),
                       (-2.5, "-2.5"), (1.5e-7, "1.5e-07"), (123.456, "123.456"), (1e300, "1e+300")] {
            assert_eq!(float_repr(x), s);
        }
    }
}
