//! Quota model: the buckets Anthropic reports and how they are read from
//! response headers and from the zero-spend `/api/oauth/usage` endpoint.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{BUCKET_5H, BUCKET_7D, BUCKET_7D_FABLE, BUCKET_7D_SONNET};

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// One reading of a bucket: utilization 0..1 (may exceed 1 in overage) and
/// the ms timestamp at which the window resets.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Bucket {
    pub utilization: Option<f64>,
    pub reset_at: Option<i64>,
    /// When this reading was taken (ms). Family buckets only ride on that
    /// family's responses, so a spent reading must expire to be revalidated.
    pub seen_at: Option<i64>,
}

impl Bucket {
    pub fn is_reset(&self, now: i64) -> bool {
        matches!(self.reset_at, Some(r) if r <= now)
    }
    pub fn clear(&mut self) {
        *self = Bucket::default();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Quota {
    pub unified5h: Bucket,
    pub unified7d: Bucket,
    pub unified7d_fable: Bucket,
    pub unified7d_sonnet: Bucket,
    /// Every model-scoped weekly bucket the usage endpoint enumerated, keyed
    /// by its lowercased display name.
    pub scoped_weekly: BTreeMap<String, Bucket>,
    pub unified_status: Option<String>,
    pub unified_status_seen_at: Option<i64>,
    // API-key accounts
    pub tokens_limit: Option<i64>,
    pub tokens_remaining: Option<i64>,
    pub requests_limit: Option<i64>,
    pub requests_remaining: Option<i64>,
    pub resets_at: Option<String>,
    /// Paid overage information from the usage endpoint, when present.
    pub spend: Option<Value>,
}

/// How long a spent family reading is trusted before it is dropped so the
/// family can be revalidated (see docs: the `7d_oi` headers only ride on Fable
/// responses, so a spent reading is otherwise self-sealing).
pub fn family_stale_ms() -> i64 {
    std::env::var("TEAMCLAUDE_FAMILY_STALE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(30 * 60 * 1000)
}

impl Quota {
    pub fn bucket(&self, key: &str) -> Option<&Bucket> {
        match key {
            BUCKET_5H => Some(&self.unified5h),
            BUCKET_7D => Some(&self.unified7d),
            BUCKET_7D_FABLE => Some(&self.unified7d_fable),
            BUCKET_7D_SONNET => Some(&self.unified7d_sonnet),
            _ => self.scoped_weekly.get(key),
        }
    }

    pub fn bucket_mut(&mut self, key: &str) -> Option<&mut Bucket> {
        match key {
            BUCKET_5H => Some(&mut self.unified5h),
            BUCKET_7D => Some(&mut self.unified7d),
            BUCKET_7D_FABLE => Some(&mut self.unified7d_fable),
            BUCKET_7D_SONNET => Some(&mut self.unified7d_sonnet),
            _ => None,
        }
    }

    /// The utilization that gates a request on `weekly_key`: the higher of the
    /// family bucket and the shared weekly one, since family spend meters twice.
    pub fn gating_weekly(&self, weekly_key: &str) -> Option<f64> {
        let shared = self.unified7d.utilization;
        if weekly_key == BUCKET_7D {
            return shared;
        }
        let own = self.bucket(weekly_key).and_then(|b| b.utilization);
        match (own, shared) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, None) => a,
            (None, b) => b,
        }
    }

    /// Drop windows that have rolled over, and spent family readings that
    /// have gone stale. Returns true if anything changed.
    pub fn clear_expired(&mut self, now: i64, threshold_for: impl Fn(&str) -> f64) -> bool {
        let mut changed = false;
        for key in [BUCKET_5H, BUCKET_7D, BUCKET_7D_FABLE, BUCKET_7D_SONNET] {
            let b = self.bucket_mut(key).unwrap();
            if b.is_reset(now) {
                b.clear();
                changed = true;
            }
        }
        let stale = family_stale_ms();
        for key in [BUCKET_7D_FABLE, BUCKET_7D_SONNET] {
            let th = threshold_for(key);
            let b = self.bucket_mut(key).unwrap();
            if let (Some(u), Some(seen)) = (b.utilization, b.seen_at) {
                if u >= th && now - seen > stale {
                    b.clear();
                    changed = true;
                }
            }
        }
        self.scoped_weekly.retain(|_, b| !b.is_reset(now));
        if let Some(seen) = self.unified_status_seen_at {
            if now - seen > 60 * 60 * 1000 {
                self.unified_status = None;
                self.unified_status_seen_at = None;
            }
        }
        changed
    }

    /// Soonest future reset across the shared buckets (ms), if known.
    pub fn soonest_reset(&self) -> Option<i64> {
        [self.unified5h.reset_at, self.unified7d.reset_at].into_iter().flatten().min().or_else(|| self.resets_at.as_deref().and_then(parse_iso_ms))
    }

    /// Update from `anthropic-ratelimit-*` response headers (lowercased keys).
    pub fn apply_headers(&mut self, h: &BTreeMap<String, String>, now: i64) {
        let f = |k: &str| h.get(k).and_then(|v| v.trim().parse::<f64>().ok());
        let secs = |k: &str| h.get(k).and_then(|v| v.trim().parse::<i64>().ok()).map(|s| s * 1000);
        let i = |k: &str| h.get(k).and_then(|v| v.trim().parse::<i64>().ok());

        if let Some(u) = f("anthropic-ratelimit-unified-5h-utilization") {
            self.unified5h.utilization = Some(u);
            self.unified5h.seen_at = Some(now);
        }
        if let Some(r) = secs("anthropic-ratelimit-unified-5h-reset") {
            self.unified5h.reset_at = Some(r);
        }
        if let Some(u) = f("anthropic-ratelimit-unified-7d-utilization") {
            self.unified7d.utilization = Some(u);
            self.unified7d.seen_at = Some(now);
        }
        if let Some(r) = secs("anthropic-ratelimit-unified-7d-reset") {
            self.unified7d.reset_at = Some(r);
        }
        // `7d_oi` = model-scoped weekly bucket (Fable on current plans).
        if let Some(u) = f("anthropic-ratelimit-unified-7d_oi-utilization") {
            self.unified7d_fable.utilization = Some(u);
            self.unified7d_fable.seen_at = Some(now);
        }
        if let Some(r) = secs("anthropic-ratelimit-unified-7d_oi-reset") {
            self.unified7d_fable.reset_at = Some(r);
        }
        if let Some(s) = h.get("anthropic-ratelimit-unified-status") {
            self.unified_status = Some(s.clone());
            self.unified_status_seen_at = Some(now);
        }
        if let Some(v) = i("anthropic-ratelimit-tokens-limit") {
            self.tokens_limit = Some(v);
        }
        if let Some(v) = i("anthropic-ratelimit-tokens-remaining") {
            self.tokens_remaining = Some(v);
        }
        if let Some(v) = i("anthropic-ratelimit-requests-limit") {
            self.requests_limit = Some(v);
        }
        if let Some(v) = i("anthropic-ratelimit-requests-remaining") {
            self.requests_remaining = Some(v);
        }
        if let Some(r) = h.get("anthropic-ratelimit-tokens-reset").or(h.get("anthropic-ratelimit-requests-reset")) {
            self.resets_at = Some(r.clone());
        }
    }

    /// Apply a normalized `/api/oauth/usage` payload.
    pub fn apply_usage(&mut self, u: &UsagePayload, now: i64) {
        let set = |dst: &mut Bucket, src: &Option<Bucket>| {
            if let Some(b) = src {
                if b.utilization.is_some() {
                    dst.utilization = b.utilization;
                    dst.seen_at = Some(now);
                }
                dst.reset_at = b.reset_at;
            }
        };
        set(&mut self.unified5h, &u.five_hour);
        set(&mut self.unified7d, &u.seven_day);
        if u.scoped_listed {
            // Upstream enumerated the scoped caps: a missing family has no cap.
            match &u.seven_day_fable {
                Some(_) => set(&mut self.unified7d_fable, &u.seven_day_fable),
                None => self.unified7d_fable.clear(),
            }
            match &u.seven_day_sonnet {
                Some(_) => set(&mut self.unified7d_sonnet, &u.seven_day_sonnet),
                None => self.unified7d_sonnet.clear(),
            }
            self.scoped_weekly = u.scoped_weekly.iter().map(|(k, b)| (k.clone(), Bucket { seen_at: Some(now), ..*b })).collect();
        } else {
            set(&mut self.unified7d_fable, &u.seven_day_fable);
            set(&mut self.unified7d_sonnet, &u.seven_day_sonnet);
        }
        if u.spend.is_some() {
            self.spend = u.spend.clone();
        }
    }

    /// API-key style used fraction, if the headers were present.
    pub fn tokens_used(&self) -> Option<f64> {
        match (self.tokens_limit, self.tokens_remaining) {
            (Some(l), Some(r)) if l > 0 => Some(1.0 - r as f64 / l as f64),
            _ => None,
        }
    }
    pub fn requests_used(&self) -> Option<f64> {
        match (self.requests_limit, self.requests_remaining) {
            (Some(l), Some(r)) if l > 0 => Some(1.0 - r as f64 / l as f64),
            _ => None,
        }
    }
}

/// Paid-overage ("extra usage") information, normalised like the original:
/// `{ enabled, usedMinor, limitMinor, currency, exponent, userDisabled, disabledReason }`.
pub fn normalize_spend(data: &Value) -> Option<Value> {
    let extra = data.get("extra_usage").filter(|v| v.is_object());
    let spend = data.get("spend").filter(|v| v.is_object());
    if extra.is_none() && spend.is_none() {
        return None;
    }
    let money = |m: Option<&Value>| -> Option<f64> {
        let m = m?.as_object()?;
        let v = m.get("amount_minor")?;
        v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    };
    let used = spend.and_then(|s| s.get("used"));
    let limit = spend.and_then(|s| s.get("limit"));
    let currency = used
        .and_then(|u| u.get("currency"))
        .or_else(|| limit.and_then(|l| l.get("currency")))
        .or_else(|| extra.and_then(|e| e.get("currency")))
        .and_then(Value::as_str)
        .map(str::to_string);
    let exponent = used
        .and_then(|u| u.get("exponent"))
        .or_else(|| limit.and_then(|l| l.get("exponent")))
        .or_else(|| extra.and_then(|e| e.get("decimal_places")))
        .and_then(Value::as_i64)
        .unwrap_or(2);
    Some(serde_json::json!({
        "enabled": extra.and_then(|e| e.get("is_enabled")).and_then(Value::as_bool).unwrap_or(false),
        "usedMinor": money(used),
        "limitMinor": money(limit),
        "currency": currency,
        "exponent": exponent,
        "userDisabled": extra.and_then(|e| e.get("user_disabled")).and_then(Value::as_bool).unwrap_or(false),
        "disabledReason": extra.and_then(|e| e.get("disabled_reason")).and_then(Value::as_str),
    }))
}

pub fn parse_iso_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis())
}

/// Normalized `/api/oauth/usage` payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsagePayload {
    pub five_hour: Option<Bucket>,
    pub seven_day: Option<Bucket>,
    pub seven_day_sonnet: Option<Bucket>,
    pub seven_day_fable: Option<Bucket>,
    pub scoped_weekly: BTreeMap<String, Bucket>,
    pub scoped_listed: bool,
    pub spend: Option<Value>,
}

fn normalize_bucket(v: Option<&Value>) -> Option<Bucket> {
    let v = v?.as_object()?;
    let pct = v
        .get("used_percentage")
        .or_else(|| v.get("utilization"))
        .or_else(|| v.get("usedPercentage"))
        .and_then(|p| p.as_f64().or_else(|| p.as_str().and_then(|s| s.parse().ok())));
    let reset = v.get("resets_at").or_else(|| v.get("resetsAt")).or_else(|| v.get("reset_at")).or_else(|| v.get("resetAt")).and_then(|r| {
        if let Some(n) = r.as_f64() {
            Some(if n < 1e12 { (n * 1000.0) as i64 } else { n as i64 })
        } else if let Some(s) = r.as_str() {
            if let Ok(n) = s.trim().parse::<f64>() {
                Some(if n < 1e12 { (n * 1000.0) as i64 } else { n as i64 })
            } else {
                parse_iso_ms(s)
            }
        } else {
            None
        }
    });
    Some(Bucket { utilization: pct.map(|p| p / 100.0), reset_at: reset, seen_at: None })
}

pub fn normalize_usage(data: &Value) -> UsagePayload {
    let mut scoped = BTreeMap::new();
    let listed = data.get("limits").map(Value::is_array).unwrap_or(false);
    if let Some(limits) = data.get("limits").and_then(Value::as_array) {
        for l in limits {
            if l.get("group").and_then(Value::as_str) != Some("weekly") {
                continue;
            }
            let Some(name) = l.pointer("/scope/model/display_name").and_then(Value::as_str) else { continue };
            if let Some(b) = normalize_bucket(Some(l)) {
                scoped.insert(name.to_ascii_lowercase(), b);
            }
        }
    }
    let find = |needle: &str| scoped.iter().find(|(k, _)| k.contains(needle)).map(|(_, b)| *b);
    let spend = normalize_spend(data);
    UsagePayload {
        five_hour: normalize_bucket(data.get("five_hour")),
        seven_day: normalize_bucket(data.get("seven_day")),
        seven_day_sonnet: normalize_bucket(data.get("seven_day_sonnet")).or_else(|| find("sonnet")),
        seven_day_fable: find("fable"),
        scoped_weekly: scoped,
        scoped_listed: listed,
        spend,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_feed_buckets() {
        let mut q = Quota::default();
        let mut h = BTreeMap::new();
        h.insert("anthropic-ratelimit-unified-5h-utilization".into(), "0.42".into());
        h.insert("anthropic-ratelimit-unified-5h-reset".into(), "1700000000".into());
        h.insert("anthropic-ratelimit-unified-7d_oi-utilization".into(), "1.02".into());
        h.insert("anthropic-ratelimit-unified-status".into(), "allowed_warning".into());
        q.apply_headers(&h, 5);
        assert_eq!(q.unified5h.utilization, Some(0.42));
        assert_eq!(q.unified5h.reset_at, Some(1_700_000_000_000));
        assert_eq!(q.unified7d_fable.utilization, Some(1.02));
        assert_eq!(q.gating_weekly(BUCKET_7D_FABLE), Some(1.02));
        assert_eq!(q.unified_status.as_deref(), Some("allowed_warning"));
    }

    #[test]
    fn usage_payload() {
        let v: Value = serde_json::json!({
            "five_hour": {"used_percentage": 12.5, "resets_at": "2026-09-07T12:00:00Z"},
            "seven_day": {"used_percentage": 50, "resets_at": 1800000000},
            "limits": [
                {"group":"weekly","scope":{"model":{"display_name":"Fable"}},"used_percentage": 90, "resets_at": 1800000000}
            ]
        });
        let u = normalize_usage(&v);
        assert_eq!(u.five_hour.unwrap().utilization, Some(0.125));
        assert_eq!(u.seven_day.unwrap().reset_at, Some(1_800_000_000_000));
        assert!(u.scoped_listed);
        assert_eq!(u.seven_day_fable.unwrap().utilization, Some(0.9));
        assert!(u.seven_day_sonnet.is_none());
        let mut q = Quota::default();
        q.unified7d_sonnet.utilization = Some(0.5);
        q.apply_usage(&u, 1);
        assert_eq!(q.unified7d_sonnet.utilization, None, "unlisted family cap is dropped");
    }

    #[test]
    fn expired_windows_clear() {
        let mut q = Quota { unified5h: Bucket { utilization: Some(0.99), reset_at: Some(10), seen_at: Some(1) }, ..Default::default() };
        assert!(q.clear_expired(11, |_| 0.98));
        assert_eq!(q.unified5h.utilization, None);
    }
}
