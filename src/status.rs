//! Human-readable rendering of the `/teamclaude/status` payload for the CLI.

use serde_json::Value;

fn pct(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{:>4.0}%", x * 100.0),
        None => "   ?".into(),
    }
}

pub fn bar(v: Option<f64>, width: usize) -> String {
    match v {
        None => "·".repeat(width),
        Some(u) => {
            let filled = ((u.clamp(0.0, 1.0)) * width as f64).round() as usize;
            format!("{}{}", "█".repeat(filled), "░".repeat(width.saturating_sub(filled)))
        }
    }
}

pub fn countdown(secs: Option<i64>) -> String {
    match secs {
        None => "-".into(),
        Some(s) if s <= 0 => "now".into(),
        Some(s) => {
            let (d, h, m) = (s / 86400, (s % 86400) / 3600, (s % 3600) / 60);
            if d > 0 {
                format!("{d}d{h:02}h")
            } else if h > 0 {
                format!("{h}h{m:02}m")
            } else {
                format!("{m}m")
            }
        }
    }
}

pub fn render(st: &Value) -> String {
    let mut out = String::new();
    let current = st.get("current").and_then(Value::as_str).unwrap_or("-");
    out.push_str(&format!(
        "TeamClaude v{}  uptime {}  current: {}\n",
        st.get("version").and_then(Value::as_str).unwrap_or("?"),
        countdown(st.get("uptimeSeconds").and_then(Value::as_i64)),
        current
    ));
    out.push_str(&format!(
        "upstream: {}   via: {}   threshold: {}   distribute: {}\n",
        st.get("upstream").and_then(Value::as_str).unwrap_or("?"),
        st.get("upstreamProxy").and_then(Value::as_str).unwrap_or("direct"),
        st.get("switchThreshold").map(|t| t.to_string()).unwrap_or_default(),
        st.get("distributeSessions").and_then(Value::as_bool).unwrap_or(false)
    ));
    if let Some(s) = st.get("sessions") {
        out.push_str(&format!(
            "sessions: {} active, {} known, {} in flight\n",
            s.get("active").and_then(Value::as_u64).unwrap_or(0),
            s.get("known").and_then(Value::as_u64).unwrap_or(0),
            s.get("inFlight").and_then(Value::as_u64).unwrap_or(0)
        ));
    }
    out.push('\n');
    out.push_str(&format!("{:<28} {:>3} {:<8} {:<20} {:<20} {:<20} {}\n", "ACCOUNT", "PRI", "STATUS", "5H", "7D", "7D-FABLE", "NOTE"));
    if let Some(accts) = st.get("accounts").and_then(Value::as_array) {
        for a in accts {
            let name = a.get("name").and_then(Value::as_str).unwrap_or("?");
            let name = crate::security::safe_text(name, 26);
            let marker = if a.get("current").and_then(Value::as_bool).unwrap_or(false) { "►" } else { " " };
            let q = |b: &str| {
                let u = a.pointer(&format!("/quota/{b}/utilization")).and_then(Value::as_f64);
                let r = a.pointer(&format!("/quota/{b}/resetInSeconds")).and_then(Value::as_i64);
                format!("{} {} {:<6}", bar(u, 6), pct(u), countdown(r))
            };
            let status = if a.get("disabled").and_then(Value::as_bool).unwrap_or(false) {
                "disabled".to_string()
            } else {
                a.get("status").and_then(Value::as_str).unwrap_or("?").to_string()
            };
            let note = a.get("blocked").and_then(Value::as_str).map(|s| crate::security::safe_text(s, 60)).unwrap_or_default();
            out.push_str(&format!(
                "{marker}{:<27} {:>3} {:<8} {} {} {} {}\n",
                name,
                a.get("priority").and_then(Value::as_i64).unwrap_or(0),
                status,
                q("unified5h"),
                q("unified7d"),
                q("unified7dFable"),
                note
            ));
        }
    }
    if let Some(routes) = st.get("routes").and_then(Value::as_array) {
        if !routes.is_empty() {
            out.push_str("\nroutes:\n");
            for r in routes {
                out.push_str(&format!(
                    "  {} match={} accounts={} pinned={}\n",
                    r.get("name").and_then(Value::as_str).unwrap_or("?"),
                    r.get("match").map(|m| m.to_string()).unwrap_or_default(),
                    r.get("accounts").map(|m| m.to_string()).unwrap_or_default(),
                    r.get("pinned").and_then(Value::as_str).unwrap_or("-")
                ));
            }
        }
    }
    if let Some(clients) = st.get("clients").and_then(Value::as_object) {
        if !clients.is_empty() {
            out.push_str("\nclients:\n");
            for (k, v) in clients {
                out.push_str(&format!(
                    "  {:<20} requests={} in={} out={} cache_read={}\n",
                    crate::security::safe_text(k, 20),
                    v.get("totalRequests").and_then(Value::as_u64).unwrap_or(0),
                    v.get("inputTokens").and_then(Value::as_i64).unwrap_or(0),
                    v.get("outputTokens").and_then(Value::as_i64).unwrap_or(0),
                    v.get("cacheReadTokens").and_then(Value::as_i64).unwrap_or(0)
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bars_and_countdowns() {
        assert_eq!(bar(Some(0.5), 4), "██░░");
        assert_eq!(bar(None, 3), "···");
        assert_eq!(countdown(Some(90061)), "1d01h");
        assert_eq!(countdown(Some(3700)), "1h01m");
        assert_eq!(countdown(Some(0)), "now");
    }
}
