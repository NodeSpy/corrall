//! The sessions table: its columns, what goes in each cell, and which
//! columns survive a narrow terminal. Shared by the dashboard (`tui.rs`) and
//! `corrall status --sessions`, which print the same rows from the same
//! document (`GET /corrall/sessions`).

use serde_json::Value;

use crate::security::safe_text;
use crate::status::format_duration;

/// The columns of the sessions table, in the order they are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCol {
    Mark,
    Id,
    Name,
    Account,
    Model,
    Last,
    Age,
    Requests,
    Input,
    CacheRead,
    CacheWrite,
    Output,
    Hit,
    Title,
}

impl SessionCol {
    pub const ALL: [SessionCol; 14] = {
        use SessionCol::*;
        [Mark, Id, Name, Account, Model, Last, Age, Requests, Input, CacheRead, CacheWrite, Output, Hit, Title]
    };
    /// Given up, first to last, as the terminal narrows.
    pub const DROP: [SessionCol; 9] = {
        use SessionCol::*;
        [Title, Input, CacheWrite, Age, Id, Model, CacheRead, Hit, Requests]
    };

    pub fn width(self) -> u16 {
        use SessionCol::*;
        match self {
            Mark => 1,
            Id => 9,
            Name => 22,
            Account => 26,
            Model => 12,
            Last => 8,
            Age => 6,
            Requests => 5,
            Input | Output => 6,
            CacheRead | CacheWrite => 7,
            Hit => 4,
            // The rest of the row; this is the least it is worth showing in.
            Title => 16,
        }
    }

    pub fn head(self) -> &'static str {
        use SessionCol::*;
        match self {
            Mark => "",
            Id => "session",
            Name => "name",
            Account => "account",
            Model => "model",
            Last => "last",
            Age => "age",
            Requests => "req",
            Input => "in",
            CacheRead => "cache r",
            CacheWrite => "cache w",
            Output => "out",
            Hit => "hit",
            Title => "title",
        }
    }

    pub fn numeric(self) -> bool {
        use SessionCol::*;
        matches!(self, Requests | Input | CacheRead | CacheWrite | Output | Hit)
    }

    /// The columns that fit in `width` cells (borders and gaps included).
    pub fn fitting(width: u16) -> Vec<SessionCol> {
        let mut cols = Self::ALL.to_vec();
        let needed = |cols: &[SessionCol]| cols.iter().map(|c| c.width() + 1).sum::<u16>() + 1;
        for drop in Self::DROP {
            if needed(&cols) <= width {
                break;
            }
            cols.retain(|c| *c != drop);
        }
        cols
    }

    pub fn cell(self, s: &Value, multi: bool, now: i64) -> String {
        use SessionCol::*;
        let text = |k: &str, max: usize| s.get(k).and_then(Value::as_str).map(|v| safe_text(v, max));
        let tokens = |k: &str| s.pointer(&format!("/tokens/{k}")).and_then(Value::as_i64).map(token_count).unwrap_or_default();
        let busy = s.get("inFlight").and_then(Value::as_u64).unwrap_or(0) > 0;
        match self {
            Mark if busy => SPINNER[(now / 250).rem_euclid(SPINNER.len() as i64) as usize].to_string(),
            Mark => " ".into(),
            Id => text("id", 8).unwrap_or_default(),
            Name => {
                // Unnamed (names off, or not one of this machine's sessions):
                // the id still tells the rows apart when its column is gone.
                let name = text("name", 40).or_else(|| text("id", 8)).unwrap_or_else(|| "-".into());
                match text("client", 16) {
                    Some(c) => format!("[{c}] {name}"),
                    None => name,
                }
            }
            Account => {
                let account = text("account", 40).unwrap_or_else(|| "-".into());
                match text("pool", 24).filter(|_| multi) {
                    Some(p) => format!("{p}/{account}"),
                    None => account,
                }
            }
            Model => text("model", 60).map(|m| short_model(&m)).unwrap_or_default(),
            Last => match s.get("busyMs").and_then(Value::as_i64).filter(|_| busy) {
                Some(ms) => format!("{}…", format_duration(ms)),
                None => s.get("idleMs").and_then(Value::as_i64).map(|ms| format!("{} ago", format_duration(ms))).unwrap_or_default(),
            },
            Age => s.get("ageMs").and_then(Value::as_i64).map(format_duration).unwrap_or_default(),
            Requests => s.get("requests").and_then(Value::as_u64).unwrap_or(0).to_string(),
            Input => tokens("input"),
            CacheRead => tokens("cacheRead"),
            CacheWrite => tokens("cacheCreation"),
            Output => tokens("output"),
            Hit => s.get("cacheHit").and_then(Value::as_f64).map(|h| format!("{:.0}%", h * 100.0)).unwrap_or_else(|| "-".into()),
            Title => text("title", 80).unwrap_or_default(),
        }
    }
}

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A token count in a few characters: `536`, `1.7k`, `89.0M`. The capital
/// keeps millions apart from the minutes in the columns beside it.
pub fn token_count(n: i64) -> String {
    match n {
        1_000_000.. => format!("{:.1}M", n as f64 / 1_000_000.0),
        1_000.. => format!("{:.1}k", n as f64 / 1_000.0),
        _ => n.to_string(),
    }
}

/// A model id short enough for a column: no `claude-` prefix, no date suffix.
pub fn short_model(model: &str) -> String {
    let m = model.strip_prefix("claude-").unwrap_or(model);
    match m.rsplit_once('-') {
        Some((head, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => head.to_string(),
        _ => m.to_string(),
    }
}

/// What stands in the mark column where nothing animates.
const BUSY: &str = "●";

/// The sessions document as plain text, for a terminal `width` cells wide:
/// a count line, a header, one line per session. `multi` is whether there is
/// more than one pool to tell apart.
pub fn render_text(doc: &Value, multi: bool, width: u16) -> String {
    let count = |k: &str| doc.get(k).and_then(Value::as_u64).unwrap_or(0);
    let mut out = format!("sessions: {} active, {} in the last hour", count("active"), count("known"));
    if doc.get("names").and_then(Value::as_bool) == Some(false) {
        out.push_str(" (names are off: corrall titles on)");
    }
    let rows = doc.get("sessions").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    if rows.is_empty() {
        return out;
    }
    let shown = count("shown") as usize;
    if (count("known") as usize) > shown && shown > 0 {
        out.push_str(&format!(", the {shown} most recent listed"));
    }
    // No borders here, so the table may use the two cells they would take.
    let cols = SessionCol::fitting(width.saturating_add(2));
    let fit = |c: SessionCol, text: &str| {
        let w = c.width() as usize;
        let text: String =
            if text.chars().count() > w && c != SessionCol::Title { text.chars().take(w.saturating_sub(1)).chain(['…']).collect() } else { text.to_string() };
        match c {
            // Last on the line and as long as it likes.
            SessionCol::Title => text,
            _ if c.numeric() => format!("{text:>w$}"),
            _ => format!("{text:<w$}"),
        }
    };
    let line = |cells: Vec<String>| cells.join(" ").trim_end().to_string();
    out.push('\n');
    out.push_str(&line(cols.iter().map(|c| fit(*c, c.head())).collect()));
    for s in rows {
        let busy = s.get("inFlight").and_then(Value::as_u64).unwrap_or(0) > 0;
        let cells = cols.iter().map(|c| match c {
            SessionCol::Mark => (if busy { BUSY } else { " " }).to_string(),
            _ => fit(*c, &c.cell(s, multi, 0)),
        });
        out.push('\n');
        out.push_str(&line(cells.collect()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn columns_are_given_up_as_the_terminal_narrows() {
        use SessionCol::*;
        assert_eq!(SessionCol::fitting(200), SessionCol::ALL);
        assert_eq!(SessionCol::fitting(150), SessionCol::ALL);
        assert_eq!(SessionCol::fitting(149), SessionCol::ALL[..13], "the title goes first");
        assert_eq!(SessionCol::fitting(120), [Mark, Id, Name, Account, Model, Last, Age, Requests, CacheRead, Output, Hit]);
        assert_eq!(SessionCol::fitting(80), [Mark, Name, Account, Last, Requests, Output, Hit]);
        assert_eq!(SessionCol::fitting(20), [Mark, Name, Account, Last, Output], "what is left when nothing fits");
    }

    #[test]
    fn model_names_and_token_counts_are_shortened_for_a_column() {
        assert_eq!(short_model("claude-fable-5-2"), "fable-5-2");
        assert_eq!(short_model("claude-opus-5-5-20260101"), "opus-5-5");
        assert_eq!(short_model("gpt-5-codex"), "gpt-5-codex");
        assert_eq!([token_count(536), token_count(1_700), token_count(89_000_000)], ["536", "1.7k", "89.0M"]);
    }

    #[test]
    fn text_table_lists_sessions_for_a_plain_terminal() {
        let doc = json!({ "active": 1, "known": 2, "names": true, "shown": 2, "sessions": [
            { "id": "2174c65f-1111-4222-8333-444455556666", "pool": "work", "name": "rosterstream-e0", "title": "Fix the roster sync retry and its very long tail",
              "account": "a@example.com", "model": "claude-fable-5-2", "active": true, "inFlight": 1, "busyMs": 12_000, "idleMs": 12_000, "ageMs": 7_260_000,
              "requests": 271, "tokens": { "input": 536, "output": 292_000, "cacheRead": 89_000_000, "cacheCreation": 939_500 }, "cacheHit": 0.99 },
            { "id": "b3acd49f-1111-4222-8333-444455556666", "pool": "default", "name": "a-session-with-a-name-far-too-long-for-its-column", "account": "b@example.com",
              "model": "claude-opus-5-5", "active": false, "inFlight": 0, "idleMs": 660_000, "ageMs": 5_820_000, "requests": 3,
              "tokens": { "input": 6, "output": 1_200, "cacheRead": 1_100_000, "cacheCreation": 507_100 }, "cacheHit": 0.68 },
        ]});
        let text = render_text(&doc, true, 200);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "sessions: 1 active, 2 in the last hour");
        assert!(lines[1].trim_start().starts_with("session   name"), "{}", lines[1]);
        assert!(lines[2].starts_with("● 2174c65f… rosterstream-e0"), "{}", lines[2]);
        for cell in ["work/a@example.com", "fable-5-2", "12s…", "271", "89.0M", "99%", "Fix the roster sync retry and its very long tail"] {
            assert!(lines[2].contains(cell), "missing {cell}: {}", lines[2]);
        }
        assert!(lines[3].starts_with("  b3acd49f… a-session-with-a-name… "), "an over-long cell is cut, not pushed into the next: {}", lines[3]);
        assert!(lines[3].contains("11m ago") && lines[3].contains("68%"), "{}", lines[3]);
        // Columns line up: the header's `req` ends where each row's count ends.
        let end = |l: &str, needle: &str| l.find(needle).map(|i| l[..i + needle.len()].chars().count());
        assert_eq!(end(lines[1], " req"), end(lines[2], " 271"));
        assert_eq!(end(lines[1], " req"), end(lines[3], "   3"));

        // Narrow: columns go, the rows stay.
        let narrow = render_text(&doc, false, 80);
        assert!(narrow.lines().all(|l| l.chars().count() <= 80), "{narrow}");
        assert!(narrow.contains("rosterstream-e0") && !narrow.contains("Fix the roster"));

        let none = render_text(&json!({ "active": 0, "known": 0, "names": false, "sessions": [] }), false, 120);
        assert_eq!(none, "sessions: 0 active, 0 in the last hour (names are off: corrall titles on)");
        let capped = render_text(&json!({ "active": 150, "known": 150, "shown": 1, "names": true, "sessions": [{ "id": "x" }] }), false, 120);
        assert!(capped.starts_with("sessions: 150 active, 150 in the last hour, the 1 most recent listed\n"), "{capped}");
    }
}
