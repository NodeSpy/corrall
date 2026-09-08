//! Request-body rewrites applied before forwarding, all operating on one parsed
//! JSON object so a body is parsed at most once per request: `account_uuid`
//! alignment, model mapping, field stripping, and orphaned tool_use/tool_result
//! repair. Every function is a no-op on any shape surprise.

use std::collections::{BTreeMap, HashSet};

use bytes::Bytes;
use serde_json::{Map, Value};

pub type Obj = Map<String, Value>;

pub fn parse(body: &[u8]) -> Option<Obj> {
    match serde_json::from_slice::<Value>(body).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

pub fn encode(m: &Obj) -> Bytes {
    Bytes::from(serde_json::to_vec(&Value::Object(m.clone())).unwrap_or_default())
}

/// Claude Code sends `metadata.user_id` as a JSON string that itself embeds
/// `"account_uuid":"<uuid>"`. Rewrite that uuid to the serving account's so
/// upstream attributes the request to the token it carries.
pub fn patch_account_uuid(m: &mut Obj, new_uuid: &str) -> bool {
    if new_uuid.len() != 36 {
        return false;
    }
    let Some(Value::String(user_id)) = m.get("metadata").and_then(|md| md.get("user_id")) else { return false };
    let needle = "\"account_uuid\":\"";
    let Some(pos) = user_id.find(needle) else { return false };
    let start = pos + needle.len();
    let Some(end) = user_id[start..].find('"').map(|e| start + e) else { return false };
    if &user_id[start..end] == new_uuid {
        return false;
    }
    let patched = format!("{}{}{}", &user_id[..start], new_uuid, &user_id[end..]);
    if let Some(Value::Object(md)) = m.get_mut("metadata") {
        md.insert("user_id".into(), Value::String(patched));
        return true;
    }
    false
}

/// Rewrite the top-level `model` through an account's model map.
pub fn rewrite_model(m: &mut Obj, map: &BTreeMap<String, String>) -> bool {
    if map.is_empty() {
        return false;
    }
    let Some(Value::String(model)) = m.get("model") else { return false };
    let Some(target) = map.get(model) else { return false };
    let t = target.clone();
    m.insert("model".into(), Value::String(t));
    true
}

/// Drop configured top-level fields (third-party upstreams reject some).
pub fn strip_fields(m: &mut Obj, fields: &[String]) -> bool {
    let mut changed = false;
    for f in fields {
        if m.remove(f).is_some() {
            changed = true;
        }
    }
    changed
}

fn tool_use_ids(blocks: &[Value]) -> Vec<String> {
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|b| b.get("id").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// Remove orphaned `tool_use` blocks (no matching `tool_result` in the next
/// user turn) and orphaned `tool_result` blocks (no preceding `tool_use`), so a
/// compacted or interrupted transcript cannot wedge the session with a
/// non-retryable 400.
pub fn sanitize_tool_pairs(m: &mut Obj) -> bool {
    let Some(Value::Array(messages)) = m.get("messages").cloned() else { return false };
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut changed = false;
    for (i, msg) in messages.iter().enumerate() {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let Some(Value::Array(blocks)) = msg.get("content") else {
            out.push(msg.clone());
            continue;
        };
        let kept: Vec<Value> = match role {
            "assistant" => {
                let uses = tool_use_ids(blocks);
                if uses.is_empty() {
                    out.push(msg.clone());
                    continue;
                }
                let results: HashSet<String> = messages
                    .get(i + 1)
                    .filter(|n| n.get("role").and_then(Value::as_str) == Some("user"))
                    .and_then(|n| n.get("content"))
                    .and_then(Value::as_array)
                    .map(|bl| {
                        bl.iter()
                            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                            .filter_map(|b| b.get("tool_use_id").and_then(Value::as_str).map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                blocks
                    .iter()
                    .filter(|b| {
                        b.get("type").and_then(Value::as_str) != Some("tool_use")
                            || b.get("id").and_then(Value::as_str).map(|id| results.contains(id)).unwrap_or(false)
                    })
                    .cloned()
                    .collect()
            }
            "user" => {
                let prev_uses: HashSet<String> = out
                    .last()
                    .filter(|p| p.get("role").and_then(Value::as_str) == Some("assistant"))
                    .and_then(|p| p.get("content"))
                    .and_then(Value::as_array)
                    .map(|bl| tool_use_ids(bl).into_iter().collect())
                    .unwrap_or_default();
                blocks
                    .iter()
                    .filter(|b| {
                        b.get("type").and_then(Value::as_str) != Some("tool_result")
                            || b.get("tool_use_id").and_then(Value::as_str).map(|id| prev_uses.contains(id)).unwrap_or(false)
                    })
                    .cloned()
                    .collect()
            }
            _ => {
                out.push(msg.clone());
                continue;
            }
        };
        if kept.len() == blocks.len() {
            out.push(msg.clone());
            continue;
        }
        changed = true;
        if kept.is_empty() {
            continue; // nothing left in this turn
        }
        let mut nm = msg.clone();
        nm["content"] = Value::Array(kept);
        out.push(nm);
    }
    if changed {
        m.insert("messages".into(), Value::Array(out));
    }
    changed
}

/// Everything the forwarder applies for an Anthropic-shaped account, in one
/// pass over an already-parsed body. Returns the bytes to send.
pub fn apply_all(
    original: &Bytes,
    parsed: Option<&Obj>,
    account_uuid: Option<&str>,
    anthropic_shaped: bool,
    model_map: &BTreeMap<String, String>,
    strip: &[String],
) -> Bytes {
    let Some(p) = parsed else { return original.clone() };
    let mut m = p.clone();
    let mut changed = false;
    if anthropic_shaped {
        changed |= sanitize_tool_pairs(&mut m);
        if let Some(u) = account_uuid {
            changed |= patch_account_uuid(&mut m, u);
        }
    }
    changed |= rewrite_model(&mut m, model_map);
    changed |= strip_fields(&mut m, strip);
    if changed {
        encode(&m)
    } else {
        original.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(s: &str) -> Obj {
        parse(s.as_bytes()).unwrap()
    }

    #[test]
    fn uuid_patch() {
        let uuid_a = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let uuid_b = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let mut m = obj(&format!(r#"{{"model":"m","metadata":{{"user_id":"{{\"device_id\":\"x\",\"account_uuid\":\"{uuid_a}\"}}"}}}}"#));
        assert!(patch_account_uuid(&mut m, uuid_b));
        let s = String::from_utf8(encode(&m).to_vec()).unwrap();
        assert!(s.contains(uuid_b) && !s.contains(uuid_a));
        assert!(!patch_account_uuid(&mut m, "short"));
        assert!(!patch_account_uuid(&mut obj("{}"), uuid_b));
    }

    #[test]
    fn model_map_and_strip() {
        let mut m = obj(r#"{"model":"claude-sonnet-4-6","context_management":{},"x":1}"#);
        let mut map = BTreeMap::new();
        map.insert("claude-sonnet-4-6".to_string(), "deepseek-v4".to_string());
        assert!(rewrite_model(&mut m, &map));
        assert_eq!(m["model"], "deepseek-v4");
        assert!(strip_fields(&mut m, &["context_management".into()]));
        assert!(!m.contains_key("context_management"));
        assert!(!strip_fields(&mut m, &["nope".into()]));
    }

    #[test]
    fn tool_pairs() {
        let mut m = obj(r#"{"messages":[
              {"role":"assistant","content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"x","input":{}}]},
              {"role":"user","content":[{"type":"text","text":"next"}]}
            ]}"#);
        assert!(sanitize_tool_pairs(&mut m));
        assert_eq!(m["messages"][0]["content"].as_array().unwrap().len(), 1);
        let mut ok = obj(r#"{"messages":[
              {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"x","input":{}}]},
              {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"r"}]}
            ]}"#);
        assert!(!sanitize_tool_pairs(&mut ok));
        let mut orphan =
            obj(r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"zz","content":"r"},{"type":"text","text":"k"}]}]}"#);
        assert!(sanitize_tool_pairs(&mut orphan));
        assert_eq!(orphan["messages"][0]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn apply_all_is_identity_when_nothing_changes() {
        let b = Bytes::from_static(br#"{"model":"claude-opus-5","messages":[]}"#);
        let p = parse(&b);
        assert_eq!(apply_all(&b, p.as_ref(), None, true, &BTreeMap::new(), &[]), b);
        assert_eq!(apply_all(&Bytes::from_static(b"not json"), None, None, true, &BTreeMap::new(), &[]), Bytes::from_static(b"not json"));
    }
}
