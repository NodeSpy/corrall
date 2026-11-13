//! The dashboard driven key by key against a server's own state, with no
//! terminal: what the account keys, the remove question and the settings
//! screen do to the running fleet and to the config file.

use std::sync::Arc;

use corrall::config::{AccountConfig, AccountType, Config, DistributeBy, DEFAULT_POOL};
use corrall::pools::Pools;
use corrall::prober::Prober;
use corrall::proxy::server::{Ctx, CtxInner, Metrics};
use corrall::tui::{Backend, Tui};
use crossterm::event::KeyCode;

fn account(name: &str, priority: i32) -> AccountConfig {
    AccountConfig {
        name: name.into(),
        kind: AccountType::Oauth,
        access_token: Some(format!("tok-{name}")),
        expires_at: Some(chrono::Utc::now().timestamp_millis() + 3_600_000),
        priority,
        ..Default::default()
    }
}

/// A dashboard on a server that is this process: the config file is a
/// temporary one, seeded with two accounts.
fn dashboard() -> (Tui, Ctx) {
    let dir = std::env::temp_dir().join(format!("corrall-dashboard-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("CORRALL_CONFIG", dir.join("corrall.json"));
    let mut seed = Config::default();
    seed.proxy.api_key = "tc-dashboard-0123456789abcdef".into();
    seed.pool_mut(DEFAULT_POOL).unwrap().accounts = vec![account("a@x.com", 0), account("b@x.com", 1)];
    seed.ensure_account_ids();
    let cfg = Config::update(|c| {
        *c = seed.clone();
        Ok(())
    })
    .unwrap();
    let pools = Pools::new(&cfg);
    let (activity, _) = tokio::sync::broadcast::channel(64);
    let ctx = Ctx(Arc::new(CtxInner {
        pools: pools.clone(),
        config: parking_lot::RwLock::new(Arc::new(cfg.clone())),
        logger: None,
        activity,
        reload: None,
        prober: None,
        metrics: Metrics::default(),
        tls: parking_lot::RwLock::new(None),
        titles: corrall::titles::Titles::new(&cfg.session_titles),
        oauth_flows: Default::default(),
        history: Default::default(),
    }));
    (Tui::new(Backend::Local { ctx: ctx.clone(), prober: Prober::new(pools) }, None), ctx)
}

fn on_disk(name: &str) -> Option<AccountConfig> {
    Config::load().unwrap().unwrap().pool(DEFAULT_POOL).unwrap().accounts.iter().find(|a| a.name == name).cloned()
}

async fn keys(tui: &mut Tui, keys: &[KeyCode]) {
    for k in keys {
        assert!(!tui.press(*k).await, "{k:?} should not quit");
    }
}

fn typed(text: &str) -> Vec<KeyCode> {
    text.chars().map(KeyCode::Char).collect()
}

/// One test: the config file is one per process.
#[tokio::test]
async fn the_dashboard_manages_accounts_and_settings() {
    let (mut tui, ctx) = dashboard();
    const W: u16 = 150;
    let line =
        |screen: &str, needle: &str| screen.lines().find(|l| l.contains(needle)).unwrap_or_else(|| panic!("no line with {needle:?} in\n{screen}")).to_string();

    // ── the cursor and the account keys ──
    let screen = tui.screen_text(W, 30);
    assert!(screen.contains("a@x.com") && screen.contains("b@x.com"), "{screen}");
    assert!(screen.contains("Enter detail") && screen.contains("g settings"), "the footer lists the keys:\n{screen}");

    keys(&mut tui, &[KeyCode::Down, KeyCode::Char('d')]).await;
    assert!(on_disk("b@x.com").unwrap().disabled, "d disables the account under the cursor");
    assert!(!on_disk("a@x.com").unwrap().disabled);
    assert!(line(&tui.screen_text(W, 30), "b@x.com").contains("disabled"), "and the table shows it");
    assert!(tui.screen_text(W, 30).contains("disabled b@x.com"));

    keys(&mut tui, &[KeyCode::Char('d'), KeyCode::Char('-'), KeyCode::Char('-'), KeyCode::Char('+')]).await;
    let b = on_disk("b@x.com").unwrap();
    assert_eq!((b.disabled, b.priority), (false, 0), "enabled again; 1 - 1 - 1 + 1");

    // s switches the running fleet to it.
    keys(&mut tui, &[KeyCode::Char('s')]).await;
    assert_eq!(ctx.pools.default().status(false)["current"], "b@x.com");
    assert!(tui.screen_text(W, 30).contains("switched to b@x.com"));

    // ── detail, and the question before a removal ──
    keys(&mut tui, &[KeyCode::Enter]).await;
    let screen = tui.screen_text(W, 40);
    assert!(screen.contains("┌ b@x.com ") && screen.contains("state                 ready, current"), "{screen}");
    assert!(screen.contains("kind                  oauth"), "{screen}");

    keys(&mut tui, &[KeyCode::Char('x')]).await;
    let screen = tui.screen_text(W, 40);
    assert!(screen.contains("Remove b@x.com from pool default?"), "{screen}");
    keys(&mut tui, &[KeyCode::Char('n')]).await;
    assert!(on_disk("b@x.com").is_some(), "n keeps it");
    assert!(tui.screen_text(W, 40).contains("┌ b@x.com "), "and stays on its screen");

    keys(&mut tui, &[KeyCode::Char('x'), KeyCode::Char('y')]).await;
    assert!(on_disk("b@x.com").is_none(), "y removes it from the config file");
    assert!(on_disk("a@x.com").is_some());
    let screen = tui.screen_text(W, 30);
    assert!(screen.contains("removed b@x.com from pool default") && screen.contains("┌ accounts "), "back on the dashboard, saying so:\n{screen}");

    // ── settings ──
    keys(&mut tui, &[KeyCode::Char('g')]).await;
    let screen = tui.screen_text(W, 30);
    assert!(screen.contains("┌ settings: ") && screen.contains("corrall.json"), "{screen}");
    assert!(line(&screen, "Spread new sessions").contains("  off  "), "{screen}");

    // Row 2 steps: off, by sessions, by usage.
    keys(&mut tui, &[KeyCode::Down, KeyCode::Down, KeyCode::Enter, KeyCode::Enter]).await;
    let pool = Config::load().unwrap().unwrap().pool(DEFAULT_POOL).unwrap().clone();
    assert_eq!((pool.distribute_sessions, pool.distribute_by), (true, DistributeBy::Usage));
    assert!(line(&tui.screen_text(W, 30), "Spread new sessions").contains("on (by usage)"));

    // Row 3 is typed: the probe interval. A value under the floor is refused
    // and stays open; a good one is saved.
    keys(&mut tui, &[KeyCode::Down, KeyCode::Enter]).await;
    assert!(line(&tui.screen_text(W, 30), "Quota probe interval").contains("> off▏"), "opens on the current value");
    keys(&mut tui, &[KeyCode::Backspace, KeyCode::Backspace, KeyCode::Backspace]).await;
    keys(&mut tui, &typed("5")).await;
    keys(&mut tui, &[KeyCode::Enter]).await;
    let screen = tui.screen_text(W, 30);
    assert!(screen.contains("minimum probe interval is 30 seconds") && line(&screen, "Quota probe interval").contains("> 5▏"), "{screen}");
    assert_eq!(Config::load().unwrap().unwrap().pool(DEFAULT_POOL).unwrap().quota_probe_seconds, 0, "nothing written");
    keys(&mut tui, &typed("00")).await;
    keys(&mut tui, &[KeyCode::Enter]).await;
    assert_eq!(Config::load().unwrap().unwrap().pool(DEFAULT_POOL).unwrap().quota_probe_seconds, 500);
    let screen = tui.screen_text(W, 30);
    assert!(line(&screen, "Quota probe interval").contains("500s") && screen.contains("saved"), "{screen}");

    // Esc on an edit leaves the value alone.
    keys(&mut tui, &[KeyCode::Enter]).await;
    keys(&mut tui, &typed("9")).await;
    keys(&mut tui, &[KeyCode::Esc]).await;
    assert_eq!(Config::load().unwrap().unwrap().pool(DEFAULT_POOL).unwrap().quota_probe_seconds, 500);

    // Session names: a toggle further down, outside the pool.
    keys(&mut tui, &[KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Enter]).await;
    assert!(Config::load().unwrap().unwrap().session_titles.enabled);

    // A row that runs something asks first, and `n` does nothing.
    keys(&mut tui, &[KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Enter]).await;
    let screen = tui.screen_text(W, 30);
    assert!(screen.contains("Point Claude Code at http://127.0.0.1:") && screen.contains("y yes"), "{screen}");
    keys(&mut tui, &[KeyCode::Esc]).await;
    assert!(tui.screen_text(W, 30).contains("┌ settings: "), "still on settings, nothing run");

    keys(&mut tui, &[KeyCode::Esc]).await;
    assert!(tui.screen_text(W, 30).contains("┌ accounts "));
    assert!(tui.press(KeyCode::Char('q')).await, "q on the dashboard quits");
}
