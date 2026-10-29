//! Decoder for Messenger's "Lightspeed" (LS) payloads.
//!
//! The web client receives a JSON-encoded program: nested arrays whose first
//! element is an opcode. The only opcode that carries data is `5`
//! (call stored procedure) — e.g. `[5, "upsertMessage", "hi", [9], [19, "80"], ...]`.
//! We flatten the program into the list of procedure calls in document order
//! and let the store apply them (all procedures are idempotent upserts, so
//! ignoring control flow such as `23` (if) is safe).

use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

/// A decoded LS argument.
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// Nested expression we don't evaluate (variable loads, native ops...).
    Expr,
}

impl Arg {
    pub fn str(&self) -> Option<&str> {
        match self {
            Arg::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }
    pub fn string(&self) -> String {
        self.str().unwrap_or_default().to_string()
    }
    pub fn int(&self) -> i64 {
        match self {
            Arg::Int(i) => *i,
            Arg::Float(f) => *f as i64,
            Arg::Str(s) => s.parse().unwrap_or(0),
            Arg::Bool(b) => *b as i64,
            _ => 0,
        }
    }
    pub fn opt_int(&self) -> Option<i64> {
        match self {
            Arg::Null | Arg::Expr => None,
            _ => Some(self.int()),
        }
    }
    pub fn bool(&self) -> bool {
        match self {
            Arg::Bool(b) => *b,
            Arg::Int(i) => *i != 0,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Call {
    pub name: String,
    pub args: Vec<Arg>,
}

impl Call {
    pub fn arg(&self, i: usize) -> &Arg {
        static NULL: Arg = Arg::Null;
        self.args.get(i).unwrap_or(&NULL)
    }
}

fn decode_arg(v: &Value) -> Arg {
    match v {
        Value::Null => Arg::Null,
        Value::Bool(b) => Arg::Bool(*b),
        Value::Number(n) => n
            .as_i64()
            .map(Arg::Int)
            .unwrap_or_else(|| Arg::Float(n.as_f64().unwrap_or(0.0))),
        Value::String(s) => Arg::Str(s.nfc().collect()),
        Value::Array(a) => match a.first().and_then(Value::as_i64) {
            Some(9) => Arg::Null,
            Some(19) => Arg::Int(
                a.get(1)
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
            ),
            _ => Arg::Expr,
        },
        Value::Object(_) => Arg::Expr,
    }
}

fn walk(v: &Value, out: &mut Vec<Call>) {
    let Value::Array(a) = v else { return };
    if a.first().and_then(Value::as_i64) == Some(5) {
        if let Some(name) = a.get(1).and_then(Value::as_str) {
            out.push(Call {
                name: name.to_string(),
                args: a[2..].iter().map(decode_arg).collect(),
            });
        }
    }
    for child in a {
        walk(child, out);
    }
}

/// Decode an LS payload string (`{"name":..,"step":[...]}`) into procedure calls.
pub fn decode_payload(payload: &str) -> anyhow::Result<Vec<Call>> {
    let v: Value = serde_json::from_str(payload)?;
    let mut out = Vec::new();
    walk(v.get("step").unwrap_or(&v), &mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_nested_calls() {
        let p = r#"{"name":null,"step":[1,[1,[4,0,1,[5,"upsertMessage","hello",[9],[19,"80"],[19,"123"],true]]],[23,[2,0],[1,[5,"deleteMessage",[19,"5"],"mid.$x"]]]]}"#;
        let calls = decode_payload(p).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "upsertMessage");
        assert_eq!(calls[0].arg(0).str(), Some("hello"));
        assert_eq!(calls[0].arg(1), &Arg::Null);
        assert_eq!(calls[0].arg(3).int(), 123);
        assert!(calls[0].arg(4).bool());
        assert_eq!(calls[1].arg(1).str(), Some("mid.$x"));
        assert_eq!(calls[1].arg(99), &Arg::Null);
    }
}
