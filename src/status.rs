//! Human-readable rendering of `/corrall/status`, a 1:1 port of the
//! original renderer: header rows, routing table, one block per account with
//! gradient quota bars, model eligibility, blocked reason, spend, usage and
//! probe rows, then per-client and per-dimension usage.

use serde_json::Value;

use crate::security::safe_text;

const ESC: &str = "\x1b[";
const RESET: &str = "\x1b[0m";
const BAR_WIDTH: usize = 18;

#[derive(Clone, Copy)]
struct Paint {
    on: bool,
}

impl Paint {
    fn wrap(&self, code: &str, v: &str) -> String {
        if self.on {
            format!("{ESC}{code}m{v}{RESET}")
        } else {
            v.to_string()
        }
    }
    fn rgb(&self, r: u8, g: u8, b: u8, v: &str) -> String {
        if self.on {
            format!("{ESC}38;2;{r};{g};{b}m{v}{RESET}")
        } else {
            v.to_string()
        }
    }
    fn bold(&self, v: &str) -> String {
        self.wrap("1", v)
    }
    fn dim(&self, v: &str) -> String {
        self.wrap("2", v)
    }
    fn gray(&self, v: &str) -> String {
        self.wrap("90", v)
    }
    fn green(&self, v: &str) -> String {
        self.wrap("32", v)
    }
    fn yellow(&self, v: &str) -> String {
        self.wrap("33", v)
    }
    fn red(&self, v: &str) -> String {
        self.wrap("31", v)
    }
    fn blue(&self, v: &str) -> String {
        self.wrap("34", v)
    }
    fn magenta(&self, v: &str) -> String {
        self.wrap("35", v)
    }
    fn cyan(&self, v: &str) -> String {
        self.wrap("36", v)
    }
    fn route(&self, color: Option<&str>, v: &str) -> String {
        match color.map(|c| c.to_ascii_lowercase()).as_deref() {
            Some("red") => self.red(v),
            Some("green") => self.green(v),
            Some("yellow") => self.yellow(v),
            Some("blue") => self.blue(v),
            Some("magenta") => self.magenta(v),
            _ => self.cyan(v),
        }
    }
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}
fn f(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(Value::as_f64)
}
fn i(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(0)
}
fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}
fn ts(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    v.as_str().and_then(crate::quota::parse_iso_ms)
}
fn pad(v: &str, w: usize) -> String {
    let n = v.chars().count();
    if n >= w {
        v.to_string()
    } else {
        format!("{v}{}", " ".repeat(w - n))
    }
}

/// Utilization of a bucket in the status JSON (`quota.<key>.utilization`).
fn util(q: &Value, key: &str) -> Option<f64> {
    q.pointer(&format!("/{key}/utilization")).and_then(Value::as_f64)
}
fn reset(q: &Value, key: &str) -> Option<i64> {
    ts(q.pointer(&format!("/{key}/resetAt")))
}

pub fn format_duration(ms: i64) -> String {
    if ms < 0 {
        return "-".into();
    }
    let total_seconds = ((ms as f64 / 1000.0).round() as i64).max(1);
    if total_seconds < 60 {
        return format!("{total_seconds}s");
    }
    let total_minutes = (total_seconds + 59) / 60;
    if total_minutes < 60 {
        return format!("{total_minutes}m");
    }
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    if hours < 24 {
        return if minutes > 0 { format!("{hours}h{minutes}m") } else { format!("{hours}h") };
    }
    let days = hours / 24;
    let rem = hours % 24;
    if rem > 0 {
        format!("{days}d{rem}h")
    } else {
        format!("{days}d")
    }
}

fn format_ago(t: i64, now: i64) -> String {
    let delta = now - t;
    if delta < 0 {
        format!("in {}", format_duration(-delta))
    } else {
        format!("{} ago", format_duration(delta))
    }
}

pub fn format_percent(v: Option<f64>) -> String {
    match v {
        None => "-".into(),
        Some(x) if !x.is_finite() => "-".into(),
        Some(x) => {
            let tenths = (x * 1000.0).round() / 10.0;
            if tenths.fract() == 0.0 {
                format!("{}%", tenths as i64)
            } else {
                format!("{tenths}%")
            }
        }
    }
}

fn format_number(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn gradient(index: usize, width: usize) -> (u8, u8, u8) {
    let t = if width <= 1 { 1.0 } else { index as f64 / (width - 1) as f64 };
    let (from, to, p) =
        if t < 0.5 { ([35.0, 209.0, 96.0], [245.0, 185.0, 40.0], t * 2.0) } else { ([245.0, 185.0, 40.0], [239.0, 68.0, 68.0], (t - 0.5) * 2.0) };
    let c = |i: usize| (from[i] + (to[i] - from[i]) * p).round() as u8;
    (c(0), c(1), c(2))
}

fn usage_bar(ratio: Option<f64>, paint: Paint, cap: Option<f64>) -> String {
    let Some(r) = ratio.filter(|r| r.is_finite()) else {
        return format!("[{}]", paint.gray(&"?".repeat(BAR_WIDTH)));
    };
    let safe = r.clamp(0.0, 1.0);
    let full = (safe * BAR_WIDTH as f64).round() as usize;
    let mut cells: Vec<String> = (0..BAR_WIDTH)
        .map(|idx| {
            if idx >= full {
                paint.gray("░")
            } else {
                let (r, g, b) = gradient(idx, BAR_WIDTH);
                paint.rgb(r, g, b, "█")
            }
        })
        .collect();
    if let Some(c) = cap.filter(|c| *c > 0.0 && *c < 1.0) {
        let at = ((c * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH - 1);
        cells[at] = if safe >= c { paint.red("┃") } else { paint.yellow("┃") };
    }
    format!("[{}]", cells.join(""))
}

/// Compact bar used by the TUI.
pub fn bar(v: Option<f64>, width: usize) -> String {
    match v {
        None => "·".repeat(width),
        Some(u) => {
            let filled = ((u.clamp(0.0, 1.0)) * width as f64).round() as usize;
            format!("{}{}", "█".repeat(filled), "░".repeat(width.saturating_sub(filled)))
        }
    }
}

/// Short countdown used by the TUI.
pub fn countdown(secs: Option<i64>) -> String {
    match secs {
        None => "-".into(),
        Some(s) if s <= 0 => "now".into(),
        Some(s) => format_duration(s * 1000),
    }
}

fn cap_for(account: &Value, bucket: &str) -> Option<f64> {
    let mu = account.get("maxUsage")?;
    if let Some(n) = mu.as_f64() {
        return Some(n);
    }
    mu.get(bucket).or_else(|| mu.get("default")).and_then(Value::as_f64)
}

fn threshold_for(threshold: &Value, bucket: &str) -> Option<f64> {
    if let Some(n) = threshold.as_f64() {
        return Some(n);
    }
    threshold.get(bucket).or_else(|| threshold.get("default")).and_then(Value::as_f64).or(Some(0.98))
}

fn format_threshold(threshold: &Value, paint: Paint) -> String {
    if let Some(n) = threshold.as_f64() {
        return format_percent(Some(n));
    }
    let Some(t) = threshold.as_object() else { return "-".into() };
    let mut parts: Vec<String> = t.iter().map(|(k, v)| format!("{k} {}", format_percent(v.as_f64()))).collect();
    parts.sort();
    paint.dim("per bucket: ").to_string() + &parts.join(", ")
}

fn format_sessions(sessions: &Value, paint: Paint) -> String {
    let active = i(sessions, "active");
    let known = i(sessions, "known");
    let mode = if b(sessions, "distribute") {
        paint.green("distributing")
    } else if i(sessions, "draining") > 0 {
        paint.yellow(&format!("draining {}", i(sessions, "draining")))
    } else {
        paint.dim("single-account")
    };
    format!("{active} active / {known} known {} {mode}", paint.dim("·"))
}

fn format_probe_summary(probe: &Value, now: i64, paint: Paint) -> String {
    if !b(probe, "enabled") {
        return paint.gray("off (passive only)");
    }
    let mut bits = vec![format!("on every {}", format_duration(i(probe, "intervalSeconds") * 1000))];
    if b(probe, "running") {
        bits.push(paint.yellow("running"));
    }
    if let Some(last) = ts(probe.get("lastRunFinishedAt")) {
        bits.push(format!("last {}", format_ago(last, now)));
    }
    if let Some(next) = ts(probe.get("nextRunAt")) {
        if next > now {
            bits.push(format!("next {}", format_duration(next - now)));
        }
    }
    bits.join(", ")
}

fn format_account_status(account: &Value, now: i64, paint: Paint) -> String {
    let mut parts = Vec::new();
    if b(account, "disabled") {
        parts.push(paint.gray("disabled"));
    }
    let status = s(account, "status").unwrap_or("unknown");
    parts.push(match status {
        "active" => paint.green(status),
        "throttled" => paint.yellow(status),
        "error" | "exhausted" => paint.red(status),
        other => other.to_string(),
    });
    if let Some(t) = ts(account.get("rateLimitedUntil")) {
        if t > now {
            parts.push(format!("throttle {}", format_duration(t - now)));
        }
    }
    if let Some(t) = ts(account.get("entitlementDeniedUntil")) {
        if t > now {
            parts.push(paint.yellow(&format!("entitlement cooldown {}", format_duration(t - now))));
        }
    }
    parts.join(" / ")
}

fn render_account_header(account: &Value, current: Option<&str>, paint: Paint, now: i64) -> String {
    let name = safe_text(s(account, "name").unwrap_or("?"), 80);
    let is_current = Some(name.as_str()) == current;
    let marker = if is_current { paint.cyan(">") } else { " ".into() };
    let shown = if is_current { paint.bold(&name) } else { name.clone() };
    let kind = match (s(account, "provider"), s(account, "type")) {
        (Some("codex"), _) => "codex",
        (_, Some(t)) => t,
        _ => "?",
    };
    let status = format_account_status(account, now, paint);
    let org = s(account, "orgName").map(|o| format!(" {}", paint.dim(&safe_text(o, 40)))).unwrap_or_default();
    let sess = match i(account, "sessions") {
        0 => String::new(),
        n => format!(" {}", paint.dim(&format!("{n} sess"))),
    };
    format!("{marker} {shown} {} {status}{org}{sess}", paint.dim(&format!("({kind}, prio {})", i(account, "priority"))))
}

fn format_quota_line(label: &str, ratio: Option<f64>, reset_at: Option<i64>, now: i64, paint: Paint, cap: Option<f64>) -> String {
    let reset = match reset_at {
        Some(r) if r > now => format!(" reset {}", format_duration(r - now)),
        _ => String::new(),
    };
    let cap_text = match cap {
        None => String::new(),
        Some(c) => {
            let reached = ratio.map(|r| r >= c).unwrap_or(false);
            let t = format!("cap {}", format_percent(Some(c)));
            format!(" {}", if reached { paint.red(&t) } else { paint.yellow(&t) })
        }
    };
    format!("{} {} {}{cap_text}{reset}", paint.dim(&pad(label, 8)), usage_bar(ratio, paint, cap), format_percent(ratio))
}

fn quota_lines(account: &Value, now: i64, paint: Paint) -> Vec<String> {
    let q = account.get("quota").cloned().unwrap_or(Value::Null);
    let mut lines = Vec::new();
    let (u5, u7, us, uf) = (util(&q, "unified5h"), util(&q, "unified7d"), util(&q, "unified7dSonnet"), util(&q, "unified7dFable"));
    if u5.is_some() || u7.is_some() || us.is_some() || uf.is_some() {
        lines.push(format_quota_line("Session", u5, reset(&q, "unified5h"), now, paint, cap_for(account, "unified5h")));
        lines.push(format_quota_line("Weekly", u7, reset(&q, "unified7d"), now, paint, cap_for(account, "unified7d")));
        if us.is_some() {
            lines.push(format_quota_line("Sonnet", us, reset(&q, "unified7dSonnet"), now, paint, cap_for(account, "unified7dSonnet")));
        }
        if uf.is_some() {
            lines.push(format_quota_line("Fable", uf, reset(&q, "unified7dFable"), now, paint, cap_for(account, "unified7dFable")));
        }
        return lines;
    }
    let resets_at = ts(q.get("resetsAt"));
    if let Some(t) = f(&q, "tokensUsed") {
        lines.push(format_quota_line("Tokens", Some(t), resets_at, now, paint, cap_for(account, "tokens")));
    }
    if let Some(r) = f(&q, "requestsUsed") {
        lines.push(format_quota_line("Requests", Some(r), resets_at, now, paint, cap_for(account, "requests")));
    }
    if lines.is_empty() {
        lines.push(format!("{} {}", paint.dim(&pad("Quota", 8)), paint.gray("unknown")));
    }
    lines
}

fn family_blocked(blocked: &[String], family: &str) -> bool {
    blocked.iter().any(|p| crate::model::glob_matches(p, family) || p.to_ascii_lowercase().contains(family))
}

fn model_routing_line(account: &Value, threshold: &Value, blocked: &[String], now: i64, paint: Paint) -> Option<String> {
    let q = account.get("quota")?;
    let us = util(q, "unified7dSonnet");
    let uf = util(q, "unified7dFable");
    if us.is_none() && uf.is_none() {
        return None;
    }
    let over_th = |v: Option<f64>, bucket: &str| v.zip(threshold_for(threshold, bucket)).map(|(v, t)| v >= t).unwrap_or(false);
    let over_cap = |v: Option<f64>, bucket: &str| v.zip(cap_for(account, bucket)).map(|(v, c)| v >= c).unwrap_or(false);
    let u5 = util(q, "unified5h");
    let u7 = util(q, "unified7d");
    let shared_over = over_th(u5, "unified5h") || over_cap(u5, "unified5h") || over_cap(u7, "unified7d");
    let cell = |label: &str, bucket: &str| -> String {
        if family_blocked(blocked, &label.to_ascii_lowercase()) {
            return format!("{label} {}{}", paint.red("⊘"), paint.dim(" blocked"));
        }
        let own = util(q, bucket);
        let gating = if bucket == "unified7d" {
            u7
        } else {
            match (own, u7) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, None) => a,
                (None, b) => b,
            }
        };
        let weekly_over = over_th(gating, bucket) || over_cap(own, bucket);
        let mark = if shared_over || weekly_over { paint.red("✗") } else { paint.green("✓") };
        let mut over: Vec<Option<i64>> = Vec::new();
        if over_th(own, bucket) {
            over.push(reset(q, bucket));
        }
        if bucket != "unified7d" && over_th(u7, "unified7d") {
            over.push(reset(q, "unified7d"));
        }
        let reset_ts = if !over.is_empty() && over.iter().all(Option::is_some) { over.iter().flatten().max().copied() } else { None };
        let when = match reset_ts {
            Some(r) if weekly_over && r > now => paint.dim(&format!(" {}", format_duration(r - now))),
            _ => String::new(),
        };
        format!("{label} {mark}{when}")
    };
    let mut cells = vec![cell("Opus", "unified7d")];
    if us.is_some() {
        cells.push(cell("Sonnet", "unified7dSonnet"));
    }
    if uf.is_some() {
        cells.push(cell("Fable", "unified7dFable"));
    }
    Some(format!("{} {}", paint.dim(&pad("Models", 8)), cells.join("   ")))
}

fn unavailable_text(key: &str, raw: &str) -> String {
    match key {
        "disabled" => "disabled by operator".into(),
        "throttled" => "upstream 429 hold".into(),
        "exhausted" => "marked exhausted".into(),
        "error" => "account error (see logs)".into(),
        "upstream-rejected" => "upstream reports quota rejected".into(),
        "quota" => "local switch threshold reached".into(),
        "capped" => "account usage cap reached (maxUsage)".into(),
        "advisor-capped" => "advisor model's usage cap reached (maxUsage)".into(),
        "entitlement" => "upstream refused this account for the organization (cooldown)".into(),
        "route" => "no route allows this account".into(),
        "advisor-quota" => "advisor model's weekly bucket spent".into(),
        "advisor-route" => "no route allows the advisor model".into(),
        "upstream-refused" => "subscription token may not be sent to this upstream".into(),
        _ => safe_text(raw, 80),
    }
}

fn unavailable_line(account: &Value, paint: Paint) -> Option<String> {
    let key = s(account, "unavailable")?;
    let raw = s(account, "blocked").unwrap_or(key);
    Some(format!("{} {}", paint.dim(&pad("Blocked", 8)), paint.yellow(&unavailable_text(key, raw))))
}

fn format_money(spend: &Value) -> String {
    let currency = s(spend, "currency").unwrap_or("");
    let sym = match currency {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        _ => "",
    };
    let exponent = spend.get("exponent").and_then(Value::as_i64).unwrap_or(2).clamp(0, 6) as usize;
    let unit = |minor: Option<f64>| -> Option<String> {
        let m = minor?;
        let v = m / 10f64.powi(exponent as i32);
        let text = format!("{v:.exponent$}");
        let (int, frac) = text.split_once('.').map(|(a, b)| (a.to_string(), Some(b.to_string()))).unwrap_or((text.clone(), None));
        let mut grouped = String::new();
        for (idx, ch) in int.chars().enumerate() {
            if idx > 0 && (int.len() - idx) % 3 == 0 {
                grouped.push(',');
            }
            grouped.push(ch);
        }
        let num = match frac {
            Some(fr) => format!("{grouped}.{fr}"),
            None => grouped,
        };
        Some(if sym.is_empty() {
            if currency.is_empty() {
                num
            } else {
                format!("{num} {currency}")
            }
        } else {
            format!("{sym}{num}")
        })
    };
    let used = unit(f(spend, "usedMinor"));
    // A zero limit means "no cap", not a cap of nothing.
    let limit = unit(f(spend, "limitMinor").filter(|l| *l > 0.0));
    match (used, limit) {
        (None, None) => "unknown".into(),
        (None, Some(l)) => format!("cap {l}"),
        (Some(u), None) => u,
        (Some(u), Some(l)) => format!("{u} of {l}"),
    }
}

fn spend_line(account: &Value, paint: Paint) -> Option<String> {
    let spend = account.pointer("/quota/spend").filter(|v| v.is_object())?;
    let spent = f(spend, "usedMinor").unwrap_or(0.0) > 0.0;
    let enabled = b(spend, "enabled");
    if !enabled && !spent {
        return None;
    }
    let amount = format_money(spend);
    let label = paint.dim(&pad("Spend", 8));
    // An explicit $0 spend limit means extra usage is turned off for the
    // account, whatever the enabled flag reports — say so instead of warning
    // about billing that cannot actually happen.
    if f(spend, "limitMinor").is_some_and(|l| l == 0.0) {
        let text = if spent { format!("extra usage disabled — {amount} used this month") } else { "extra usage disabled".to_string() };
        return Some(format!("{label} {}", paint.gray(&text)));
    }
    if enabled {
        let text = if spent {
            format!("billing real money — {amount} used this month")
        } else {
            format!("can bill real money past its plan limits — {amount} used")
        };
        let t = format!("⚠ {text}");
        return Some(format!("{label} {}", if spent { paint.red(&t) } else { paint.yellow(&t) }));
    }
    let why = if b(spend, "userDisabled") {
        "now disabled by the account holder".to_string()
    } else if let Some(r) = s(spend, "disabledReason") {
        format!("now off ({})", safe_text(r, 40))
    } else {
        "now off".to_string()
    };
    Some(format!("{label} {}", paint.yellow(&format!("{amount} spent this month, {why}"))))
}

fn format_usage(usage: &Value, now: i64) -> String {
    let requests = i(usage, "totalRequests");
    let tokens = i(usage, "inputTokens") + i(usage, "outputTokens");
    let last = ts(usage.get("lastUsed")).map(|t| format!(", last {}", format_ago(t, now))).unwrap_or_default();
    format!("{requests} req, {} tok{last}", format_number(tokens))
}

fn format_account_probe(account: &Value, probe_enabled: bool, now: i64, paint: Paint) -> String {
    if !probe_enabled {
        return paint.gray("off");
    }
    let Some(row) = account.get("probe") else { return paint.gray("never") };
    let status = s(row, "status").unwrap_or("never");
    match status {
        "not-applicable" => return paint.gray("not applicable"),
        "never" => return paint.gray("never"),
        _ => {}
    }
    let colored = match status {
        "ok" => paint.green("ok"),
        "running" => paint.yellow("running"),
        other => paint.red(other),
    };
    let when = ts(row.get("lastProbedAt").or_else(|| row.get("startedAt"))).map(|t| format!(" {}", format_ago(t, now))).unwrap_or_default();
    let duration = row.get("durationMs").and_then(Value::as_i64).map(|d| format!(", {d}ms")).unwrap_or_default();
    let error = s(row, "error").map(|e| format!(", {}", safe_text(e, 80))).unwrap_or_default();
    format!("{colored}{when}{duration}{error}")
}

fn routing_lines(routes: &[Value], blocked: &[String], paint: Paint) -> Vec<String> {
    if routes.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![paint.bold("Routing")];
    for route in routes {
        let globs: Vec<String> =
            route.get("match").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
        let matched = globs.join(", ");
        let route_blocked = !globs.is_empty()
            && globs.iter().all(|g| {
                let core = g.replace('*', "").to_ascii_lowercase();
                blocked.iter().any(|p| {
                    let pc = p.replace('*', "").to_ascii_lowercase();
                    core.contains(&pc) || pc.contains(&core)
                })
            });
        let accounts = if route_blocked {
            paint.red("blocked")
        } else {
            let names: Vec<String> = route
                .get("accounts")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|x| {
                            if b(x, "eligible") {
                                paint.green(&safe_text(s(x, "name").unwrap_or("?"), 40))
                            } else {
                                paint.red(&safe_text(s(x, "name").unwrap_or("?"), 40))
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            if names.is_empty() {
                paint.gray("(none)")
            } else {
                names.join(" ")
            }
        };
        let tag = if b(route, "autocreated") {
            paint.dim(" (auto)")
        } else if let Some(bk) = s(route, "bucket") {
            paint.dim(&format!(" [{bk}]"))
        } else {
            String::new()
        };
        let pin = s(route, "pinned").map(|p| paint.dim(&format!(" [pinned: {}]", safe_text(p, 40)))).unwrap_or_default();
        let label = paint.route(s(route, "color"), &pad(&matched, 16));
        lines.push(format!("  {label} {} {accounts}{tag}{pin}", paint.dim("→")));
    }
    lines.push(String::new());
    lines
}

fn render_usage_entries(lines: &mut Vec<String>, entries: &serde_json::Map<String, Value>, paint: Paint, now: i64) {
    let mut rows: Vec<(&String, &Value)> = entries.iter().collect();
    rows.sort_by_key(|(_, c)| std::cmp::Reverse(i(c, "inputTokens") + i(c, "outputTokens")));
    for (name, c) in rows {
        let tokens = format!("{} in / {} out", format_number(i(c, "inputTokens")), format_number(i(c, "outputTokens")));
        let last = ts(c.get("lastUsed")).map(|t| format!(", last {}", format_ago(t, now))).unwrap_or_default();
        lines.push(format!("  {} {} req, {tokens}{last}", paint.cyan(&pad(&safe_text(name, 20), 20)), i(c, "totalRequests")));
    }
}

/// The rows that describe the daemon rather than any one pool.
fn daemon_lines(st: &Value, paint: Paint) -> Vec<String> {
    let empty = Value::Object(Default::default());
    let warm = st.get("warm").unwrap_or(&empty);
    let mut lines: Vec<String> = Vec::new();
    if b(warm, "enabled") {
        lines.push(format!("{} on every {}", paint.dim(&pad("Keep-warm", 12)), format_duration(i(warm, "intervalSeconds") * 1000)));
    }
    if let Some(up) = st.pointer("/server/uptimeSeconds").and_then(Value::as_i64).or_else(|| st.get("uptimeSeconds").and_then(Value::as_i64)) {
        lines.push(format!("{} up {}", paint.dim(&pad("Server", 12)), format_duration(up * 1000)));
    }
    if let Some(tag) = s(st, "updateAvailable") {
        lines.push(format!("{} {}", paint.dim(&pad("Update", 12)), paint.yellow(&format!("{} available — run: corrall update", safe_text(tag, 30)))));
    }
    lines
}

/// Render the status payload. `color` enables ANSI (truecolor bars).
///
/// The payload flattens the default pool onto the top level and repeats every
/// pool under `pools`, so a one-pool install renders exactly what it did
/// before pools existed. With several, the daemon-wide rows are printed once
/// and each pool gets its own section.
pub fn render(st: &Value, color: bool, now: i64) -> String {
    let paint = Paint { on: color };
    let Some(pools) = st.get("pools").and_then(Value::as_array).filter(|p| p.len() > 1) else {
        return render_pool(st, "Corrall status", paint, now, true);
    };
    let mut out = vec![
        paint.bold(&format!("Corrall status — {} pools", pools.len())),
        format!("{} {}", paint.dim(&pad("Default", 12)), paint.cyan(s(st, "defaultPool").unwrap_or("-"))),
    ];
    out.extend(daemon_lines(st, paint));
    for p in pools {
        let star = if b(p, "default") { " *" } else { "" };
        out.push(String::new());
        out.push(render_pool(p, &format!("pool {}{star}", safe_text(s(p, "pool").unwrap_or("?"), 24)), paint, now, false));
    }
    out.push(String::new());
    out.push(paint.dim("* serves requests with no /pool/<name> prefix"));
    out.join("\n")
}

/// One pool's section, under `heading`. `daemon` adds the daemon-wide rows,
/// which a multi-pool render has already printed once at the top.
fn render_pool(st: &Value, heading: &str, paint: Paint, now: i64, daemon: bool) -> String {
    let mut lines: Vec<String> = Vec::new();
    let empty = Value::Object(Default::default());
    let probe = st.get("probe").unwrap_or(&empty);
    let accounts: Vec<Value> = st.get("accounts").and_then(Value::as_array).cloned().unwrap_or_default();
    let blocked: Vec<String> = st
        .get("blockedModels")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).filter(|p| !p.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    let current = s(st, "currentAccount").or_else(|| s(st, "current"));
    let threshold = st.get("switchThreshold").cloned().unwrap_or(Value::Null);

    lines.push(paint.bold(heading));
    lines.push(format!("{} {}", paint.dim(&pad("Active", 12)), paint.cyan(current.unwrap_or("none"))));
    lines.push(format!("{} {}", paint.dim(&pad("Switch at", 12)), format_threshold(&threshold, paint)));
    if !blocked.is_empty() {
        lines.push(format!("{} {}", paint.dim(&pad("Blocked", 12)), paint.red(&safe_text(&blocked.join(", "), 200))));
    }
    if let Some(sessions) = st.get("sessions") {
        lines.push(format!("{} {}", paint.dim(&pad("Sessions", 12)), format_sessions(sessions, paint)));
    }
    lines.push(format!("{} {}", paint.dim(&pad("Probe", 12)), format_probe_summary(probe, now, paint)));
    if daemon {
        lines.extend(daemon_lines(st, paint));
    }
    lines.push(String::new());

    let routes: Vec<Value> = st.get("routes").and_then(Value::as_array).cloned().unwrap_or_default();
    lines.extend(routing_lines(&routes, &blocked, paint));

    let probe_enabled = b(probe, "enabled");
    for account in &accounts {
        lines.push(render_account_header(account, current, paint, now));
        for q in quota_lines(account, now, paint) {
            lines.push(format!("  {q}"));
        }
        if let Some(r) = model_routing_line(account, &threshold, &blocked, now, paint) {
            lines.push(format!("  {r}"));
        }
        if let Some(w) = unavailable_line(account, paint) {
            lines.push(format!("  {w}"));
        }
        if let Some(sp) = spend_line(account, paint) {
            lines.push(format!("  {sp}"));
        }
        lines.push(format!("  {} {}", paint.dim(&pad("Usage", 8)), format_usage(account.get("usage").unwrap_or(&empty), now)));
        lines.push(format!("  {} {}", paint.dim(&pad("Probe", 8)), format_account_probe(account, probe_enabled, now, paint)));
        lines.push(String::new());
    }

    if let Some(clients) = st.get("clients").and_then(Value::as_object).filter(|c| !c.is_empty()) {
        lines.push(paint.bold("Clients"));
        render_usage_entries(&mut lines, clients, paint, now);
        lines.push(String::new());
    }
    if let Some(dims) = st.get("usageDimensions").and_then(Value::as_object) {
        for (dimension, entries) in dims {
            let Some(rows) = entries.as_object().filter(|r| !r.is_empty()) else { continue };
            let safe = safe_text(dimension, 40);
            let mut chars = safe.chars();
            let title = match chars.next() {
                Some(c) => format!("{}{} usage", c.to_uppercase(), chars.as_str()),
                None => "Usage".into(),
            };
            lines.push(paint.bold(&title));
            render_usage_entries(&mut lines, rows, paint, now);
            lines.push(String::new());
        }
    }
    lines.join("\n").trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn durations_match_original() {
        assert_eq!(format_duration(500), "1s");
        assert_eq!(format_duration(59_000), "59s");
        assert_eq!(format_duration(61_000), "2m");
        assert_eq!(format_duration(3_900_000), "1h5m");
        assert_eq!(format_duration(3_600_000), "1h");
        assert_eq!(format_duration(90_000_000), "1d1h");
        assert_eq!(format_percent(Some(0.98)), "98%");
        assert_eq!(format_percent(Some(0.995)), "99.5%");
        assert_eq!(format_number(1234), "1.2k");
        assert_eq!(format_number(2_500_000), "2.5m");
    }

    #[test]
    fn money() {
        let sp = json!({ "enabled": true, "usedMinor": 2426.0, "limitMinor": 0.0, "currency": "USD", "exponent": 2 });
        assert_eq!(format_money(&sp), "$24.26");
        let sp = json!({ "usedMinor": 123456.0, "limitMinor": 500000.0, "currency": "EUR", "exponent": 2 });
        assert_eq!(format_money(&sp), "€1,234.56 of €5,000.00");
        let sp = json!({ "usedMinor": null, "limitMinor": null });
        assert_eq!(format_money(&sp), "unknown");
    }

    #[test]
    fn spend_zero_limit_reads_as_disabled() {
        let p = Paint { on: false };
        // $0 limit with prior spend: disabled note keeps the used amount.
        let acct = json!({ "quota": { "spend": { "enabled": true, "usedMinor": 2426.0, "limitMinor": 0.0, "currency": "USD", "exponent": 2 } } });
        assert_eq!(spend_line(&acct, p).as_deref(), Some("Spend    extra usage disabled — $24.26 used this month"));
        // $0 limit, enabled, nothing spent: bare disabled note, no billing warning.
        let acct = json!({ "quota": { "spend": { "enabled": true, "usedMinor": 0.0, "limitMinor": 0.0, "currency": "USD", "exponent": 2 } } });
        assert_eq!(spend_line(&acct, p).as_deref(), Some("Spend    extra usage disabled"));
        // $0 limit, not enabled, nothing spent: no line, as before.
        let acct = json!({ "quota": { "spend": { "enabled": false, "usedMinor": 0.0, "limitMinor": 0.0, "currency": "USD", "exponent": 2 } } });
        assert_eq!(spend_line(&acct, p), None);
    }

    #[test]
    fn update_available_shows_in_status() {
        let now = 1_000_000_000_000;
        let st = json!({
            "currentAccount": "a@x.com", "switchThreshold": 0.98,
            "probe": { "enabled": false }, "server": { "uptimeSeconds": 125 },
            "updateAvailable": "v2.2.0", "accounts": [], "routes": [],
        });
        let out = render(&st, false, now);
        assert!(out.contains("Update       v2.2.0 available — run: corrall update"), "got:\n{out}");
        // Absent field → no Update row.
        let mut st2 = st.clone();
        st2["updateAvailable"] = json!(null);
        assert!(!render(&st2, false, now).contains("Update"));
    }

    #[test]
    fn bars() {
        let p = Paint { on: false };
        assert_eq!(usage_bar(None, p, None), format!("[{}]", "?".repeat(18)));
        assert_eq!(usage_bar(Some(0.5), p, None), format!("[{}{}]", "█".repeat(9), "░".repeat(9)));
        assert!(usage_bar(Some(0.5), p, Some(0.6)).contains('┃'));
        assert_eq!(gradient(0, 18), (35, 209, 96));
        assert_eq!(gradient(17, 18), (239, 68, 68));
    }

    #[test]
    fn renders_like_the_original() {
        let now = 1_000_000_000_000;
        let st = json!({
            "currentAccount": "a@x.com",
            "switchThreshold": 0.98,
            "sessions": { "active": 1, "known": 2, "distribute": false },
            "probe": { "enabled": false },
            "server": { "uptimeSeconds": 125 },
            "routes": [ { "name": "fable", "match": ["*fable*"], "accounts": [ { "name": "a@x.com", "eligible": true } ], "autocreated": true } ],
            "accounts": [ {
                "name": "a@x.com", "type": "oauth", "priority": 0, "status": "active", "orgName": "Org", "sessions": 1,
                "quota": {
                    "unified5h": { "utilization": 0.8, "resetAt": now + 300_000 },
                    "unified7d": { "utilization": 0.24, "resetAt": now + 400_000_000 },
                    "unified7dFable": { "utilization": 0.99, "resetAt": now + 400_000_000 },
                    "unified7dSonnet": { "utilization": null },
                    "spend": { "enabled": true, "usedMinor": 1234.0, "limitMinor": 5000.0, "currency": "USD", "exponent": 2 }
                },
                "usage": { "totalRequests": 5, "inputTokens": 1200, "outputTokens": 300, "lastUsed": now - 11_000 },
                "unavailable": null
            } ],
            "clients": { "alice": { "totalRequests": 2, "inputTokens": 10, "outputTokens": 5 } }
        });
        let out = render(&st, false, now);
        let expected = "\
Corrall status
Active       a@x.com
Switch at    98%
Sessions     1 active / 2 known · single-account
Probe        off (passive only)
Server       up 3m

Routing
  *fable*          → a@x.com (auto)

> a@x.com (oauth, prio 0) active Org 1 sess
  Session  [██████████████░░░░] 80% reset 5m
  Weekly   [████░░░░░░░░░░░░░░] 24% reset 4d15h
  Fable    [██████████████████] 99% reset 4d15h
  Models   Opus ✓   Fable ✗ 4d15h
  Spend    ⚠ billing real money — $12.34 of $50.00 used this month
  Usage    5 req, 1.5k tok, last 11s ago
  Probe    off

Clients
  alice                2 req, 10 in / 5 out";
        assert_eq!(out, expected);
    }

    /// One pool renders as it always did; several put the daemon rows once at
    /// the top and give every pool its own headed section.
    #[test]
    fn several_pools_each_get_a_section() {
        let now = 1_000_000_000_000;
        let pool = |name: &str, default: bool, current: &str| {
            json!({
                "pool": name, "default": default, "currentAccount": current,
                "switchThreshold": 0.98, "probe": { "enabled": false }, "accounts": [], "routes": [],
            })
        };
        let mut st = pool("default", true, "a@x.com");
        st["defaultPool"] = json!("default");
        st["server"] = json!({ "uptimeSeconds": 125 });
        st["pools"] = json!([pool("default", true, "a@x.com"), pool("work", false, "b@y.com")]);

        let out = render(&st, false, now);
        let expected = "\
Corrall status — 2 pools
Default      default
Server       up 3m

pool default *
Active       a@x.com
Switch at    98%
Probe        off (passive only)

pool work
Active       b@y.com
Switch at    98%
Probe        off (passive only)

* serves requests with no /pool/<name> prefix";
        assert_eq!(out, expected);

        // A single pool is the pre-pools render, daemon rows inline and all.
        st["pools"] = json!([pool("default", true, "a@x.com")]);
        assert!(render(&st, false, now).starts_with("Corrall status\nActive       a@x.com"));
    }
}
