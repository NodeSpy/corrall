//! Model family detection, quota-bucket mapping, glob matching and body
//! inspection (request model, advisor model).

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    Fable,
    Sonnet,
    Opus,
    Haiku,
    Other,
}

impl Family {
    pub fn of(model: Option<&str>) -> Family {
        let Some(m) = model else { return Family::Other };
        let l = m.to_ascii_lowercase();
        if l.contains("fable") {
            Family::Fable
        } else if l.contains("sonnet") {
            Family::Sonnet
        } else if l.contains("opus") {
            Family::Opus
        } else if l.contains("haiku") {
            Family::Haiku
        } else {
            Family::Other
        }
    }

    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Family::Fable => "fable",
            Family::Sonnet => "sonnet",
            Family::Opus => "opus",
            Family::Haiku => "haiku",
            Family::Other => "other",
        }
    }
}

pub const BUCKET_5H: &str = "unified5h";
pub const BUCKET_7D: &str = "unified7d";
pub const BUCKET_7D_FABLE: &str = "unified7dFable";
pub const BUCKET_7D_SONNET: &str = "unified7dSonnet";
pub const WEEKLY_BUCKETS: &[&str] = &[BUCKET_7D, BUCKET_7D_FABLE, BUCKET_7D_SONNET];

/// The weekly bucket that governs a model.
pub fn weekly_bucket_for(model: Option<&str>) -> &'static str {
    match Family::of(model) {
        Family::Fable => BUCKET_7D_FABLE,
        Family::Sonnet => BUCKET_7D_SONNET,
        _ => BUCKET_7D,
    }
}

pub fn is_weekly_bucket(b: &str) -> bool {
    WEEKLY_BUCKETS.contains(&b)
}

/// Shell-style glob where `*` is the only wildcard; case-insensitive.
pub fn glob_matches(glob: &str, model: &str) -> bool {
    fn rec(g: &[u8], m: &[u8]) -> bool {
        match (g.first(), m.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                let rest = &g[1..];
                (0..=m.len()).any(|i| rec(rest, &m[i..]))
            }
            (Some(gc), Some(mc)) if gc.eq_ignore_ascii_case(mc) => rec(&g[1..], &m[1..]),
            _ => false,
        }
    }
    rec(glob.as_bytes(), model.as_bytes())
}

pub fn any_glob_matches(globs: &[String], model: &str) -> bool {
    globs.iter().any(|g| glob_matches(g, model))
}

/// Top-level `model` from a JSON request body.
#[allow(dead_code)]
pub fn request_model(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    v.get("model")?.as_str().map(str::to_string)
}

/// The advisor model nested in `tools[]` (`{"type":"advisor...", "model": "..."}`).
#[allow(dead_code)]
pub fn advisor_model(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let tools = v.get("tools")?.as_array()?;
    tools.iter().find_map(|t| {
        let ty = t.get("type")?.as_str()?;
        if ty.to_ascii_lowercase().starts_with("advisor") {
            t.get("model")?.as_str().map(str::to_string)
        } else {
            None
        }
    })
}

/// Both models in one parse.
pub fn models_in_body(body: &[u8]) -> (Option<String>, Option<String>) {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return (None, None) };
    let main = v.get("model").and_then(Value::as_str).map(str::to_string);
    let adv = v.get("tools").and_then(Value::as_array).and_then(|tools| {
        tools.iter().find_map(|t| {
            let ty = t.get("type")?.as_str()?;
            if ty.to_ascii_lowercase().starts_with("advisor") {
                t.get("model")?.as_str().map(str::to_string)
            } else {
                None
            }
        })
    });
    (main, adv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families() {
        assert_eq!(Family::of(Some("claude-fable-5-1")), Family::Fable);
        assert_eq!(Family::of(Some("claude-sonnet-4-6")), Family::Sonnet);
        assert_eq!(Family::of(Some("deepseek-v4")), Family::Other);
        assert_eq!(weekly_bucket_for(Some("claude-opus-5")), BUCKET_7D);
        assert_eq!(weekly_bucket_for(Some("claude-fable-5-1")), BUCKET_7D_FABLE);
    }

    #[test]
    fn globs() {
        assert!(glob_matches("*fable*", "claude-FABLE-5-1"));
        assert!(glob_matches("claude-*", "claude-opus"));
        assert!(!glob_matches("*opus*", "claude-sonnet"));
        assert!(glob_matches("*", ""));
        assert!(glob_matches("deepseek-*", "deepseek-v4-pro[1m]"));
    }

    #[test]
    fn body_models() {
        let b = br#"{"model":"claude-opus-5","tools":[{"type":"advisor_20260101","model":"claude-fable-5-1"}]}"#;
        assert_eq!(request_model(b).as_deref(), Some("claude-opus-5"));
        assert_eq!(advisor_model(b).as_deref(), Some("claude-fable-5-1"));
        assert_eq!(request_model(b"not json"), None);
    }
}
