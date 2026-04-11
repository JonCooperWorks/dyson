//! Tiny template renderer: `{{var}}` substitution only.
//!
//! No conditionals, no loops, no escaping rules — just literal
//! substitution from a context object's serialized fields. If a key is
//! missing the placeholder is replaced with an empty string.

use serde::Serialize;

#[derive(Debug)]
pub struct TemplateError(pub String);

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TemplateError {}

pub fn render_template<T: Serialize>(template: &str, ctx: &T) -> Result<String, TemplateError> {
    let value = serde_json::to_value(ctx)
        .map_err(|e| TemplateError(format!("serialize ctx: {e}")))?;
    let map = match value {
        serde_json::Value::Object(m) => m,
        _ => return Err(TemplateError("template ctx must be an object".into())),
    };

    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' && chars.peek() == Some(&'{') {
            chars.next(); // consume second '{'
            let mut key = String::new();
            let mut closed = false;
            while let Some(c) = chars.next() {
                if c == '}' && chars.peek() == Some(&'}') {
                    chars.next();
                    closed = true;
                    break;
                }
                key.push(c);
            }
            if !closed {
                return Err(TemplateError(format!("unterminated placeholder: {{{{{key}")));
            }
            let key = key.trim();
            let replacement = map
                .get(key)
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Null => String::new(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            out.push_str(&replacement);
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Ctx {
        name: String,
        n: u32,
    }

    #[test]
    fn substitutes_string_field() {
        let s = render_template("hello {{name}}", &Ctx { name: "alice".into(), n: 1 }).unwrap();
        assert_eq!(s, "hello alice");
    }

    #[test]
    fn substitutes_number_field() {
        let s = render_template("n={{n}}", &Ctx { name: "x".into(), n: 42 }).unwrap();
        assert_eq!(s, "n=42");
    }

    #[test]
    fn missing_key_is_empty() {
        let s = render_template("a={{missing}}!", &Ctx { name: "x".into(), n: 1 }).unwrap();
        assert_eq!(s, "a=!");
    }

    #[test]
    fn no_placeholders_passes_through() {
        let s = render_template("plain text", &Ctx { name: "x".into(), n: 1 }).unwrap();
        assert_eq!(s, "plain text");
    }

    #[test]
    fn unterminated_errors() {
        let err = render_template("{{name", &Ctx { name: "x".into(), n: 1 }).unwrap_err();
        assert!(err.0.contains("unterminated"));
    }
}
