use super::*;
use crate::theme::ColorMode;

fn theme() -> Theme {
    Theme::builtin("prime", ColorMode::TrueColor)
}

fn kb() -> KeybindingsManager {
    KeybindingsManager::new()
}

fn text(line: &Line) -> String {
    line.iter().map(|span| span.content.as_str()).collect()
}

const NOW_MS: i64 = 1_795_900_000_000;

/// The four providers in codexbar 0.85.0's shape (trimmed), including codex's null primary.
const USAGE_JSON: &str = r#"[
  {"provider":"codex","source":"oauth","rateWindowLabels":{"secondary":"Weekly"},
   "usage":{"accountEmail":"someone@example.com","primary":null,
     "secondary":{"usedPercent":100,"windowMinutes":10080,"resetsAt":"2026-11-26T03:59:42Z","resetDescription":"Nov 25 at 10:59 PM"},
     "tertiary":null}},
  {"provider":"claude","source":"web","rateWindowLabels":{"primary":"Session","secondary":"Weekly"},
   "usage":{"primary":{"usedPercent":16,"windowMinutes":300,"resetsAt":"2026-11-23T23:19:59Z"},
     "secondary":{"usedPercent":96,"windowMinutes":10080,"resetsAt":"2026-11-23T22:59:59Z"}}},
  {"provider":"cursor","source":"web","rateWindowLabels":{"primary":"Total","secondary":"Cursor","tertiary":"Third Party"},
   "usage":{"primary":{"usedPercent":17.3,"windowMinutes":43200},
     "secondary":{"usedPercent":1.5,"windowMinutes":43200},
     "tertiary":{"usedPercent":100,"windowMinutes":43200}}},
  {"provider":"antigravity","source":"cli",
   "usage":{"primary":{"usedPercent":25.73,"windowMinutes":300,"resetsAt":"2026-11-24T01:12:47Z","resetDescription":"You have used some of your 5-hour limit, it will fully refresh in 3 hours, 46 minutes."},
     "secondary":{"usedPercent":52.84,"windowMinutes":10080,"resetsAt":"2026-11-29T07:00:51Z"}},
   "error":null},
  {"provider":"zai","source":"api","usage":null,"error":{"code":1,"message":"Missing  API\n key","kind":"auth"}},
  {"source":"cli"}
]"#;

const COST_JSON: &str = r#"{
  "provider": "claude",
  "sessionCostUSD": 4.07,
  "last30DaysCostUSD": 1220.94,
  "sessionTokens": 4810077,
  "totals": { "totalCost": 1220.94, "totalTokens": 2524856738 },
  "daily": [
    { "date": "2026-11-22", "totalCost": 100.0, "modelBreakdowns": [] },
    { "date": "2026-11-23", "totalCost": 4.07, "totalTokens": 4810077,
      "modelBreakdowns": [
        { "modelName": "claude-haiku-5-5", "cost": 0.02, "totalTokens": 108823 },
        { "modelName": "claude-opus-5-5", "cost": 4.05, "totalTokens": 4701254 },
        { "modelName": "unpriced-model", "totalTokens": 9000000 }
      ] }
  ]
}"#;

#[test]
fn parses_all_providers_with_null_windows_and_errors() {
    let providers = parse_usage(USAGE_JSON).unwrap();
    let names: Vec<&str> = providers.iter().map(|p| p.provider.as_str()).collect();
    assert_eq!(names, ["codex", "claude", "cursor", "antigravity", "zai"]);

    let codex = &providers[0];
    assert_eq!(codex.source.as_deref(), Some("oauth"));
    assert_eq!(codex.windows.len(), 1);
    assert_eq!(codex.windows[0].label, "week");
    assert!((codex.windows[0].used_percent - 100.0).abs() < f64::EPSILON);
    assert!(codex.windows[0].resets_at_ms.is_some());
    assert_eq!(codex.error, None);

    let claude = &providers[1];
    let labels: Vec<&str> = claude.windows.iter().map(|w| w.label.as_str()).collect();
    assert_eq!(labels, ["5h", "week"]);

    // Same-length windows read by the provider's own names.
    let cursor = &providers[2];
    let labels: Vec<&str> = cursor.windows.iter().map(|w| w.label.as_str()).collect();
    assert_eq!(labels, ["total", "cursor", "third party"]);

    let antigravity = &providers[3];
    assert_eq!(antigravity.windows[0].label, "5h");
    assert!(antigravity.windows[0]
        .reset_description
        .as_deref()
        .unwrap()
        .starts_with("You have used"));

    let zai = &providers[4];
    assert!(zai.windows.is_empty());
    assert_eq!(zai.error.as_deref(), Some("Missing API key"));
}

#[test]
fn usage_parse_rejects_garbage() {
    assert!(parse_usage("not json").is_err());
    assert!(parse_usage("42").is_err());
    assert_eq!(parse_usage("[]").unwrap(), Vec::new());
    // A single object is accepted as a one-provider list.
    let one =
        parse_usage(r#"{"provider":"claude","usage":{"primary":{"usedPercent":5}}}"#).unwrap();
    assert_eq!(one[0].windows[0].label, "limit");
}

#[test]
fn window_labels_follow_window_length() {
    assert_eq!(window_label(300), "5h");
    assert_eq!(window_label(360), "5h");
    assert_eq!(window_label(1440), "day");
    assert_eq!(window_label(10080), "week");
    assert_eq!(window_label(43200), "month");
}

#[test]
fn parses_cost_object_and_array_payloads() {
    let object = parse_cost(COST_JSON).unwrap();
    let array = parse_cost(&format!("[{COST_JSON}]")).unwrap();
    assert_eq!(object, array);
    assert_eq!(object.today_usd, Some(4.07));
    assert_eq!(object.last_30_days_usd, Some(1220.94));
    let models: Vec<&str> = object
        .today_models
        .iter()
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(
        models,
        ["claude-opus-5-5", "claude-haiku-5-5", "unpriced-model"]
    );
}

#[test]
fn cost_breakdown_needs_spend_on_the_newest_day() {
    // codex: no spend today, the newest row is an older day.
    let json = r#"[{"sessionCostUSD":0,"last30DaysCostUSD":291.94,
        "daily":[{"date":"2026-11-21","totalCost":12.0,
          "modelBreakdowns":[{"modelName":"gpt-6-astra","cost":12.0,"totalTokens":5}]}]}]"#;
    let cost = parse_cost(json).unwrap();
    assert_eq!(cost.today_usd, Some(0.0));
    assert!(cost.today_models.is_empty());
    // A missing 30-day field falls back to the totals.
    let totals = parse_cost(r#"{"totals":{"totalCost":3.5}}"#).unwrap();
    assert_eq!(totals.last_30_days_usd, Some(3.5));
}

#[test]
fn cost_errors_surface_as_text() {
    let error = parse_cost(
        r#"[{"provider":"cli","error":{"code":1,"message":"Error: unknown provider 'bogus'","kind":"args"}}]"#,
    );
    assert_eq!(error, Err("Error: unknown provider 'bogus'".to_string()));
    assert_eq!(
        parse_cost(r#"{"error":"rate limited"}"#),
        Err("rate limited".to_string())
    );
    assert!(parse_cost("[]").is_err());
    assert!(parse_cost("").is_err());
}

#[test]
fn bar_cells_and_color_thresholds() {
    assert_eq!(bar_cells(0.0), (String::new(), "\u{2500}".repeat(22)));
    assert_eq!(bar_cells(100.0), ("\u{2501}".repeat(22), String::new()));
    assert_eq!(bar_cells(150.0).0.chars().count(), 22);
    assert_eq!(bar_cells(-5.0).0, "");
    let (filled, empty) = bar_cells(50.0);
    assert_eq!((filled.chars().count(), empty.chars().count()), (11, 11));
    assert_eq!(bar_color(0.0), ThemeColor::Success);
    assert_eq!(bar_color(59.9), ThemeColor::Success);
    assert_eq!(bar_color(60.0), ThemeColor::Warning);
    assert_eq!(bar_color(84.9), ThemeColor::Warning);
    assert_eq!(bar_color(85.0), ThemeColor::Error);
    assert_eq!(bar_color(100.0), ThemeColor::Error);
}

#[test]
fn parses_antigravity_extra_rate_windows_and_suppresses_unknown_provider_spend() {
    let json = r#"[
      {"provider":"antigravity","source":"cli",
       "usage":{"primary":{"usedPercent":38.2,"windowMinutes":300},
         "extraRateWindows":[
           {"title":"Gemini 5-hour","window":{"usedPercent":38.2,"windowMinutes":300,"resetsAt":"2026-11-24T06:12:47Z"}},
           {"title":"Gemini weekly","window":{"usedPercent":34.4,"windowMinutes":10080,"resetsAt":"2026-11-30T20:12:47Z"}},
           {"title":"Claude/GPT 5-hour","window":{"usedPercent":100,"windowMinutes":300,"resetsAt":"2026-11-24T02:38:55Z"}},
           {"title":"Claude/GPT weekly","window":{"usedPercent":99.2,"windowMinutes":10080,"resetsAt":"2026-11-29T07:00:51Z"}}
         ]}}
    ]"#;
    let mut providers = parse_usage(json).unwrap();
    let labels: Vec<&str> = providers[0]
        .windows
        .iter()
        .map(|w| w.label.as_str())
        .collect();
    assert_eq!(
        labels,
        ["gemini 5h", "gemini week", "claude/gpt 5h", "claude/gpt week"]
    );
    providers[0].cost = Some(Err("Error: unknown provider 'antigravity'".to_string()));
    let mut view = UsageView::new(None, None, false, None);
    view.set_report(UsageReport::Loaded(providers), Instant::now());
    let rows = render(&mut view, 90, 40);
    assert!(rows.iter().any(|r| r.contains("Antigravity")));
    assert!(rows.iter().any(|r| r.contains("gemini 5h")));
    assert!(rows.iter().any(|r| r.contains("claude/gpt 5h")));
    assert!(!rows.iter().any(|r| r.contains("spend unavailable")));
}

#[test]
fn formats_money_tokens_and_countdowns() {
    assert_eq!(format_usd(0.0), "$0.00");
    assert_eq!(format_usd(4.07), "$4.07");
    assert_eq!(format_usd(1220.94), "$1,220.94");
    assert_eq!(format_usd(1_234_567.891), "$1,234,567.89");
    assert_eq!(format_tokens(950), "950");
    assert_eq!(format_tokens(63_100_000), "63.1M");
    assert_eq!(format_tokens(2_524_856_738), "2.5B");
    assert_eq!(
        format_countdown(Duration::from_secs(3 * 3600 + 46 * 60)),
        "3h 46m"
    );
    assert_eq!(
        format_countdown(Duration::from_secs(5 * 86400 + 9 * 3600)),
        "5d 9h"
    );
    assert_eq!(format_countdown(Duration::from_secs(30)), "<1m");
}

fn loaded_view() -> UsageView {
    let mut providers = parse_usage(USAGE_JSON).unwrap();
    providers[1].cost = Some(parse_cost(COST_JSON));
    providers[2].cost = Some(Err("timed out after 12s".to_string()));
    for index in [0, 3, 4] {
        providers[index].cost = Some(Ok(ProviderCost::default()));
    }
    let mut view = UsageView::new(None, None, false, Some(1.5));
    view.set_report(UsageReport::Loaded(providers), Instant::now());
    view
}

fn render(view: &mut UsageView, width: usize, rows: usize) -> Vec<String> {
    view.render_at(
        &theme(),
        width,
        &kb(),
        rows,
        NOW_MS,
        Some(Duration::from_secs(12)),
    )
    .iter()
    .map(text)
    .collect()
}

#[test]
fn renders_providers_bars_spend_and_footer() {
    let mut view = loaded_view();
    let rows = render(&mut view, 90, 80);
    assert!(
        rows[1].contains("Usage & Quotas") && rows[1].contains("updated 12s ago"),
        "{rows:#?}"
    );
    let claude = rows
        .iter()
        .find(|r| r.contains("Claude") && r.contains("web"))
        .unwrap();
    // Right-aligned with a two-column margin.
    assert!(
        claude.ends_with("today $4.07 \u{b7} 30d $1,220.94"),
        "{claude:?}"
    );
    assert_eq!(str_width(claude), 88);
    assert!(
        rows.iter()
            .any(|r| r.contains("claude-opus-5-5 4.7M \u{b7} $4.05")),
        "{rows:#?}"
    );
    assert!(rows
        .iter()
        .any(|r| r.contains("spend unavailable: timed out after 12s")));
    assert!(rows.iter().any(|r| r.contains("Missing API key")));
    assert!(
        rows.iter()
            .any(|r| r.contains("Today $4.07") && r.contains("30d $1,220.94") && r.contains("This chat $1.50")),
        "{rows:#?}"
    );
    assert!(rows.iter().any(|r| r.contains("r refresh")));
    // codex: one week bar, full.
    let week = rows
        .iter()
        .find(|r| r.contains("week") && r.contains("100%"))
        .unwrap();
    assert!(week.contains(&"\u{2501}".repeat(22)), "{week}");
    // No account details leak into the panel.
    assert!(!rows.iter().any(|r| r.contains("example.com")));
}

#[test]
fn window_rows_color_bars_by_threshold() {
    let mut view = loaded_view();
    let lines = view.render_at(&theme(), 90, &kb(), 80, NOW_MS, None);
    let t = theme();
    let bar_style = |needle: &str| {
        let line = lines.iter().find(|l| text(l).contains(needle)).unwrap();
        line.iter()
            .find(|span| span.content.contains('\u{2501}'))
            .map(|span| span.style)
    };
    // claude 5h at 16% (success) and week at 96% (error).
    assert_eq!(bar_style(" 16%"), Some(t.fg_style(ThemeColor::Success)));
    assert_eq!(bar_style(" 96%"), Some(t.fg_style(ThemeColor::Error)));
}

#[test]
fn every_row_fits_narrow_widths() {
    for width in [20, 32, 48, 64] {
        let mut view = loaded_view();
        for row in view.render_at(&theme(), width, &kb(), 80, NOW_MS, None) {
            assert!(str_width(&text(&row)) <= width, "{width}: {:?}", text(&row));
        }
    }
}

#[test]
fn reset_label_prefers_countdown_then_description() {
    let window = UsageWindow {
        label: "5h".into(),
        used_percent: 10.0,
        resets_at_ms: Some(NOW_MS + (3 * 3600 + 46 * 60) * 1000),
        reset_description: Some("long text".into()),
    };
    assert_eq!(
        reset_label(&window, NOW_MS).as_deref(),
        Some("\u{21bb} 3h 46m")
    );
    let past = UsageWindow {
        resets_at_ms: Some(NOW_MS - 1),
        ..window.clone()
    };
    assert_eq!(
        reset_label(&past, NOW_MS).as_deref(),
        Some("\u{21bb} reset due")
    );
    let described = UsageWindow {
        resets_at_ms: None,
        ..window
    };
    assert_eq!(
        reset_label(&described, NOW_MS).as_deref(),
        Some("long text")
    );
}

#[test]
fn status_tracks_loading_refresh_and_unavailable() {
    let mut view = UsageView::new(None, None, true, None);
    assert!(render(&mut view, 60, 20)[1].contains("loading\u{2026}"));
    view.set_report(UsageReport::Unavailable, Instant::now());
    let rows = render(&mut view, 80, 20);
    assert!(rows[1].contains("codexbar not found"));
    assert!(rows.iter().any(|r| r.contains("PA_CODEXBAR_BIN")));
    view.set_refreshing();
    assert!(view.is_refreshing());
    assert!(render(&mut view, 80, 20)[1].contains("refreshing\u{2026}"));
    view.set_report(UsageReport::Failed("boom".into()), Instant::now());
    let rows = render(&mut view, 80, 20);
    assert!(rows.iter().any(|r| r.contains("codexbar failed: boom")));
    assert!(!view.is_refreshing());
}

#[test]
fn unavailable_keeps_the_session_spend() {
    let mut view = UsageView::new(Some(UsageReport::Unavailable), None, false, Some(0.42));
    let rows = render(&mut view, 80, 20);
    assert!(
        rows.iter().any(|r| r.contains("This chat $0.42")),
        "{rows:#?}"
    );
}

#[test]
fn keys_scroll_refresh_and_close() {
    let mut view = loaded_view();
    let frame = render(&mut view, 90, 10);
    assert!(frame.len() <= 10);
    assert!(frame.iter().any(|r| r.contains("(1/")), "{frame:#?}");
    let kb = kb();
    assert_eq!(view.handle_key("down", &kb), UsageViewAction::None);
    assert_eq!(view.scroll, 1);
    assert_eq!(view.handle_key("j", &kb), UsageViewAction::None);
    assert_eq!(view.scroll, 2);
    assert_eq!(view.handle_key("k", &kb), UsageViewAction::None);
    assert_eq!(view.handle_key("up", &kb), UsageViewAction::None);
    assert_eq!(view.scroll, 0);
    view.handle_key("up", &kb);
    assert_eq!(view.scroll, 0);
    view.handle_key("pageDown", &kb);
    assert_eq!(view.scroll, view.visible_rows);
    view.handle_key("pageUp", &kb);
    assert_eq!(view.scroll, 0);
    for _ in 0..200 {
        view.handle_key("j", &kb);
    }
    assert_eq!(view.scroll, view.content_rows - view.visible_rows);
    assert_eq!(view.handle_key("r", &kb), UsageViewAction::Refresh);
    assert_eq!(view.handle_key("escape", &kb), UsageViewAction::Close);
    assert_eq!(view.handle_key("q", &kb), UsageViewAction::Close);
    assert_eq!(view.handle_key("ctrl+c", &kb), UsageViewAction::Close);
}

#[test]
fn finds_codexbar_by_override_path_then_fallbacks() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("codexbar");
    std::fs::write(&bin, "").unwrap();
    let other = tempfile::tempdir().unwrap();
    let fallback = other.path().join("codexbar");
    std::fs::write(&fallback, "").unwrap();
    let path = std::env::join_paths([dir.path()]).unwrap();
    assert_eq!(
        find_codexbar_in(None, Some(path.as_os_str()), &[&fallback]),
        Some(bin)
    );
    assert_eq!(
        find_codexbar_in(None, None, &[Path::new("/nonexistent/codexbar"), &fallback]),
        Some(fallback.clone())
    );
    // The override wins, and a wrong override reports not found.
    assert_eq!(
        find_codexbar_in(Some(fallback.as_os_str()), Some(path.as_os_str()), &[]),
        Some(fallback)
    );
    assert_eq!(
        find_codexbar_in(
            Some(OsStr::new("/nonexistent/codexbar")),
            Some(path.as_os_str()),
            &[]
        ),
        None
    );
    assert_eq!(find_codexbar_in(None, None, &[]), None);
}

#[test]
fn bars_render_before_spend_and_costs_merge_per_provider() {
    let mut view = UsageView::new(None, None, true, None);
    view.set_report(
        UsageReport::Loaded(parse_usage(USAGE_JSON).unwrap()),
        Instant::now(),
    );
    let rows = render(&mut view, 90, 80);
    assert!(rows[1].contains("loading spend\u{2026}"), "{rows:#?}");
    assert!(!view.is_refreshing());
    // Every bar is already drawn; every provider waits on its spend.
    assert!(rows.iter().any(|r| r.contains(" 96%")), "{rows:#?}");
    let header = |rows: &[String], name: &str| {
        rows.iter()
            .find(|r| r.contains(name) && r.contains('\u{25cf}'))
            .unwrap()
            .clone()
    };
    for name in ["Codex", "Claude", "Cursor", "Antigravity", "Z.AI"] {
        assert!(
            header(&rows, name).ends_with("spend loading\u{2026}"),
            "{name}: {rows:#?}"
        );
    }
    let t = theme();
    let lines = view.render_at(&t, 90, &kb(), 80, NOW_MS, None);
    let loading = lines
        .iter()
        .flat_map(|line| line.iter())
        .find(|span| span.content == "spend loading\u{2026}")
        .unwrap();
    assert_eq!(loading.style, t.fg_style(ThemeColor::Dim));

    // claude settles first; the others keep loading and keep their bars.
    view.set_cost(1, parse_cost(COST_JSON));
    let rows = render(&mut view, 90, 80);
    assert!(header(&rows, "Claude").ends_with("today $4.07 \u{b7} 30d $1,220.94"));
    assert!(header(&rows, "Cursor").ends_with("spend loading\u{2026}"));
    assert!(rows.iter().any(|r| r.contains(" 96%")));
    assert!(rows[1].contains("loading spend\u{2026}"));

    // An out-of-range index changes nothing.
    view.set_cost(99, Ok(ProviderCost::default()));
    view.set_cost(2, Err("timed out after 12s".to_string()));
    for index in [0, 3, 4] {
        view.set_cost(index, Ok(ProviderCost::default()));
    }
    let rows = render(&mut view, 90, 80);
    assert!(rows[1].contains("updated 12s ago"), "{rows:#?}");
    assert!(!rows.iter().any(|r| r.contains("spend loading")));
    assert!(rows
        .iter()
        .any(|r| r.contains("spend unavailable: timed out after 12s")));
    assert!(header(&rows, "Claude").ends_with("30d $1,220.94"));
}

#[test]
fn pending_costs_counts_unsettled_providers() {
    assert_eq!(UsageReport::Unavailable.pending_costs(), 0);
    assert_eq!(UsageReport::Failed("x".into()).pending_costs(), 0);
    let mut report = UsageReport::Loaded(parse_usage(USAGE_JSON).unwrap());
    assert_eq!(report.pending_costs(), 5);
    report.set_cost(0, Ok(ProviderCost::default()));
    report.set_cost(0, Err("again".into()));
    assert_eq!(report.pending_costs(), 4);
    let mut failed = UsageReport::Failed("x".into());
    failed.set_cost(0, Ok(ProviderCost::default()));
    assert_eq!(failed, UsageReport::Failed("x".into()));
}

async fn collect_events(bin: Option<PathBuf>) -> Vec<UsageEvent> {
    let mut events = Vec::new();
    load_usage_with(bin, |event| events.push(event)).await;
    events
}

#[tokio::test]
async fn loader_reports_unavailable_without_a_binary() {
    assert_eq!(
        collect_events(None).await,
        [UsageEvent::Listed(UsageReport::Unavailable)]
    );
}

#[cfg(unix)]
fn fake_codexbar(dir: &Path, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("codexbar");
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

#[cfg(unix)]
#[tokio::test]
async fn loader_streams_the_list_then_each_cost_as_it_settles() {
    let dir = tempfile::tempdir().unwrap();
    // claude's cost is slow; bogus fails at once, so its event arrives first.
    let bin = fake_codexbar(
        dir.path(),
        r#"#!/bin/sh
if [ "$1" = "usage" ]; then
  echo '[{"provider":"claude","source":"web","usage":{"primary":{"usedPercent":40,"windowMinutes":300}}},{"provider":"bogus","usage":null}]'
  exit 0
fi
if [ "$3" = "claude" ]; then
  sleep 1
  echo '{"sessionCostUSD":2.5,"last30DaysCostUSD":10}'
  exit 0
fi
echo '[{"error":{"message":"unknown provider"}}]'
exit 1
"#,
    );
    let events = collect_events(Some(bin)).await;
    assert_eq!(events.len(), 3, "{events:#?}");
    let UsageEvent::Listed(mut report) = events[0].clone() else {
        panic!("the list comes first: {events:#?}");
    };
    assert_eq!(report.pending_costs(), 2);
    assert_eq!(
        events[1],
        UsageEvent::Cost {
            index: 1,
            cost: Err("unknown provider".to_string())
        }
    );
    let UsageEvent::Cost { index: 0, cost } = events[2].clone() else {
        panic!("claude settles last: {events:#?}");
    };
    assert_eq!(cost.unwrap().today_usd, Some(2.5));
    for event in &events[1..] {
        if let UsageEvent::Cost { index, cost } = event.clone() {
            report.set_cost(index, cost);
        }
    }
    assert_eq!(report.pending_costs(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn loader_reports_a_failed_usage_call() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_codexbar(
        dir.path(),
        "#!/bin/sh\necho 'no providers configured' >&2\nexit 2\n",
    );
    assert_eq!(
        collect_events(Some(bin)).await,
        [UsageEvent::Listed(UsageReport::Failed(
            "no providers configured".to_string()
        ))]
    );
}
