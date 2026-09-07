//! Request-body rewrites applied before forwarding: `account_uuid` alignment,
//! model mapping, field stripping, and orphaned tool_use/tool_result repair.
//! Every function returns the original bytes untouched on any parse surprise.

use std::collections::{BTreeMap, HashSet};

use bytes::Bytes;
use serde_json::{Map, Value};

fn parse(body: &[u8]) -> Option<Map<String, Value>> {
    match serde_json::from_slice::<Value>(body).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

fn encode(m: Map<String, Value>) -> Bytes {
    Bytes::from(serde_json::to_vec(&Value::Object(m)).unwrap_or_default())
}

/// Claude Code sends `metadata.user_id` as a JSON string that itself embeds
/// `"account_uuid":"<uuid>"`. Rewrite that uuid to the serving account's so
/// upstream attributes the request to the token it carries.
pub fn patch_account_uuid(body: &Bytes, new_uuid: &str) -> Bytes {
    if new_uuid.len() != 36 {
        return body.clone();
    }
    let Some(mut m) = parse(body) else { return body.clone() };
    let Some(Value::String(user_id)) = m.get("metadata").and_then(|md| md.get("user_id")).cloned() else {
        return body.clone();
    };
    let needle = "\"account_uuid\":\"";
    let Some(pos) = user_id.find(needle) else { return body.clone() };
    let start = pos + needle.len();
    let Some(end) = user_id[start..].find('"').map(|e| start + e) else { return body.clone() };
    if &user_id[start..end] == new_uuid {
        return body.clone();
    }
    let patched = format!("{}{}{}", &user_id[..start], new_uuid, &user_id[end..]);
    if let Some(Value::Object(md)) = m.get_mut("metadata") {
        md.insert("user_id".into(), Value::String(patched));
    }
    encode(m)
}

/// Rewrite the top-level `model` through an account's model map.
pub fn rewrite_model(body: &Bytes, map: &BTreeMap<String, String>) -> Bytes {
    if map.is_empty() {
        return body.clone();
    }
    let Some(mut m) = parse(body) else { return body.clone() };
    let Some(Value::String(model)) = m.get("model") else { return body.clone() };
    let Some(target) = map.get(model) else { return body.clone() };
    m.insert("model".into(), Value::String(target.clone()));
    encode(m)
}

/// Drop configured top-level fields (third-party upstreams reject some).
pub fn strip_fields(body: &Bytes, fields: &[String]) -> Bytes {
    if fields.is_empty() {
        return body.clone();
    }
    let Some(mut m) = parse(body) else { return body.clone() };
    let mut changed = false;
    for f in fields {
        if m.remove(f).is_some() {
            changed = true;
        }
    }
    if changed {
        encode(m)
    } else {
        body.clone()
    }
}

/// Remove orphaned `tool_use` blocks (no matching `tool_result` in the next
/// user turn) and orphaned `tool_result` blocks (no preceding `tool_use`), so a
/// compacted or interrupted transcript cannot wedge the session with a
/// non-retryable 400.
pub fn sanitize_tool_pairs(body: &Bytes) -> Bytes {
    let Some(mut m) = parse(body) else { return body.clone() };
    let Some(Value::Array(messages)) = m.get("messages").cloned() else { return body.clone() };
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut changed = false;
    let mut i = 0;
    while i < messages.len() {
        let msg = &messages[i];
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let content = msg.get("content");
        if role == "assistant" {
            if let Some(Value::Array(blocks)) = content {
                let uses: Vec<String> = blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                    .filter_map(|b| b.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect();
                if !uses.is_empty() {
                    let results: HashSet<String> = messages
                        .get(i + 1)
                        .filter(|n| n.get("role").and_then(Value::as_str) == Some("user"))
                        .and_then(|n| n.get("content"))
                        .and_then(Value::as_array)
                        .map(|blocks| {
                            blocks
                                .iter()
                                .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                                .filter_map(|b| b.get("tool_use_id").and_then(Value::as_str).map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    let orphaned: Vec<&String> = uses.iter().filter(|u| !results.contains(*u)).collect();
                    if !orphaned.is_empty() {
                        let mut nm = msg.clone();
                        let kept: Vec<Value> = blocks
                            .iter()
                            .filter(|b| {
                                !(b.get("type").and_then(Value::as_str) == Some("tool_use")
                                    && b.get("id").and_then(Value::as_str).map(|id| orphaned.iter().any(|o| *o == id)).unwrap_or(false))
                            })
                            .cloned()
                            .collect();
                        changed = true;
                        if kept.is_empty() {
                            // Nothing left to say in this turn; drop it entirely.
                            i += 1;
                            continue;
                        }
                        nm["content"] = Value::Array(kept);
                        out.push(nm);
                        i += 1;
                        continue;
                    }
                }
            }
            out.push(msg.clone());
        } else if role == "user" {
            if let Some(Value::Array(blocks)) = content {
                let prev_uses: HashSet<String> = out
                    .last()
                    .filter(|p| p.get("role").and_then(Value::as_str) == Some("assistant"))
                    .and_then(|p| p.get("content"))
                    .and_then(Value::as_array)
                    .map(|bl| {
                        bl.iter()
                            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                            .filter_map(|b| b.get("id").and_then(Value::as_str).map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let kept: Vec<Value> = blocks
                    .iter()
                    .filter(|b| {
                        if b.get("type").and_then(Value::as_str) != Some("tool_result") {
                            return true;
                        }
                        b.get("tool_use_id").and_then(Value::as_str).map(|id| prev_uses.contains(id)).unwrap_or(false)
                    })
                    .cloned()
                    .collect();
                if kept.len() != blocks.len() {
                    changed = true;
                    if kept.is_empty() {
                        i += 1;
                        continue;
                    }
                    let mut nm = msg.clone();
                    nm["content"] = Value::Array(kept);
                    out.push(nm);
                    i += 1;
                    continue;
                }
            }
            out.push(msg.clone());
        } else {
            out.push(msg.clone());
        }
        i += 1;
    }
    if !changed {
        return body.clone();
    }
    m.insert("messages".into(), Value::Array(out));
    encode(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_patch() {
        let uuid_a = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let uuid_b = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let body = Bytes::from(format!(r#"{{"model":"m","metadata":{{"user_id":"{{\"device_id\":\"x\",\"account_uuid\":\"{uuid_a}\"}}"}}}}"#));
        let out = patch_account_uuid(&body, uuid_b);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains(uuid_b) && !s.contains(uuid_a));
        assert_eq!(patch_account_uuid(&body, "short"), body);
        assert_eq!(patch_account_uuid(&Bytes::from_static(b"{}"), uuid_b), Bytes::from_static(b"{}"));
    }

    #[test]
    fn model_map_and_strip() {
        let body = Bytes::from_static(br#"{"model":"claude-sonnet-4-6","context_management":{},"x":1}"#);
        let mut map = BTreeMap::new();
        map.insert("claude-sonnet-4-6".to_string(), "deepseek-v4".to_string());
        let out = rewrite_model(&body, &map);
        assert!(std::str::from_utf8(&out).unwrap().contains("deepseek-v4"));
        let out = strip_fields(&body, &["context_management".into()]);
        assert!(!std::str::from_utf8(&out).unwrap().contains("context_management"));
        assert_eq!(strip_fields(&body, &["nope".into()]), body);
    }

    #[test]
    fn tool_pairs() {
        let body = Bytes::from_static(
            br#"{"messages":[
              {"role":"assistant","content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"x","input":{}}]},
              {"role":"user","content":[{"type":"text","text":"next"}]}
            ]}"#,
        );
        let out = sanitize_tool_pairs(&body);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["messages"][0]["content"].as_array().unwrap().len(), 1);
        let ok = Bytes::from_static(
            br#"{"messages":[
              {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"x","input":{}}]},
              {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"r"}]}
            ]}"#,
        );
        assert_eq!(sanitize_tool_pairs(&ok), ok);
        let orphan_result = Bytes::from_static(
            br#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"zz","content":"r"},{"type":"text","text":"k"}]}]}"#,
        );
        let v: Value = serde_json::from_slice(&sanitize_tool_pairs(&orphan_result)).unwrap();
        assert_eq!(v["messages"][0]["content"].as_array().unwrap().len(), 1);
    }
}
