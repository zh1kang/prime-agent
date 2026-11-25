//! The `/usage` panel: provider rate-limit windows and API-equivalent spend read from the
//! external `codexbar` CLI. The loader runs in a background task (the panel never blocks the
//! key loop) and streams its result: the rate windows first, then each provider's spend as it
//! settles. The panel renders whatever report the session cached last.
//!
//! `codexbar usage --json-only` lists every configured provider with up to three rate windows;
//! `codexbar cost --provider <name> --json-only` returns that provider's local-log spend.
//! Both payloads are parsed leniently: unknown fields are ignored, and a field of the wrong
//! shape drops only that field.

use crate::keybindings::KeybindingsManager;
use crate::menu_panel::{hint_row, menu_list_layout, scroll_row};
use crate::theme::{Theme, ThemeColor};
use crate::width::{str_width, truncate_line, truncate_to_width};
use crate::{Line, Span};
use serde_json::Value;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The env override for the `codexbar` binary path.
pub const CODEXBAR_BIN_ENV: &str = "PA_CODEXBAR_BIN";
/// Install locations checked after `PATH`.
const CODEXBAR_FALLBACK_PATHS: [&str; 2] =
    ["/opt/homebrew/bin/codexbar", "/usr/local/bin/codexbar"];
const USAGE_TIMEOUT: Duration = Duration::from_secs(15);
const COST_TIMEOUT: Duration = Duration::from_secs(12);

const BAR_CELLS: usize = 22;
const BAR_FILLED: char = '\u{2501}'; // ━
const BAR_EMPTY: char = '\u{2500}'; // ─
/// Top padding, title, hairline, blank, hint, trailing blank.
const FIXED_FRAME_ROWS: usize = 6;
const SCROLL_INDICATOR_ROWS: usize = 1;
/// Model rows shown under a provider's spend.
const TOP_MODELS: usize = 3;

/// One rate-limit window of a provider.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageWindow {
    /// `5h`, `day`, `week`, `month`, or the provider's own window label.
    pub label: String,
    pub used_percent: f64,
    pub resets_at_ms: Option<i64>,
    pub reset_description: Option<String>,
}

/// One model's share of a day's spend.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSpend {
    pub name: String,
    pub cost_usd: Option<f64>,
    pub tokens: u64,
}

/// A provider's local-log spend (`codexbar cost`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderCost {
    /// codexbar's current-day (`session`) spend.
    pub today_usd: Option<f64>,
    pub last_30_days_usd: Option<f64>,
    /// Today's per-model spend, highest first.
    pub today_models: Vec<ModelSpend>,
}

/// One provider row: its windows, its spend, and any error codexbar reported.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderUsage {
    pub provider: String,
    pub source: Option<String>,
    pub windows: Vec<UsageWindow>,
    pub error: Option<String>,
    /// `None` until the cost fetch settles; `Err` when it failed.
    pub cost: Option<Result<ProviderCost, String>>,
}

/// The outcome of one background load.
#[derive(Debug, Clone, PartialEq)]
pub enum UsageReport {
    /// No `codexbar` binary was found.
    Unavailable,
    /// `codexbar usage` failed or returned unreadable output.
    Failed(String),
    Loaded(Vec<ProviderUsage>),
}

/// The text of a codexbar `error` field: a string, an object's `message`, or the raw JSON.
fn error_text(value: &Value) -> Option<String> {
    let text = match value {
        Value::Null => return None,
        Value::String(text) => text.clone(),
        Value::Object(map) => match map.get("message").and_then(Value::as_str) {
            Some(message) => message.to_string(),
            None => value.to_string(),
        },
        other => other.to_string(),
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

/// The window label from its length: `5h` up to 6 hours, then `day`, `week`, `month`.
fn window_label(minutes: u64) -> &'static str {
    match minutes {
        0..=360 => "5h",
        361..=1440 => "day",
        1441..=10080 => "week",
        _ => "month",
    }
}

fn parse_window(value: &Value, rate_label: Option<&str>) -> Option<UsageWindow> {
    let used_percent = value.get("usedPercent")?.as_f64()?;
    let label = match value.get("windowMinutes").and_then(Value::as_u64) {
        Some(minutes) => window_label(minutes).to_string(),
        None => rate_label.map_or_else(|| "limit".to_string(), str::to_lowercase),
    };
    Some(UsageWindow {
        label,
        used_percent,
        resets_at_ms: value
            .get("resetsAt")
            .and_then(Value::as_str)
            .and_then(pa_types::incident::timestamp_to_ms),
        reset_description: value
            .get("resetDescription")
            .and_then(Value::as_str)
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|text| !text.is_empty()),
    })
}

fn normalize_extra_label(title: &str) -> String {
    let trimmed = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = trimmed.to_lowercase();
    lower
        .replace(" 5-hour", " 5h")
        .replace(" 5 hour", " 5h")
        .replace(" weekly", " week")
        .replace(" monthly", " month")
}

fn parse_extra_windows(usage: Option<&Value>) -> Vec<UsageWindow> {
    let Some(extras) = usage
        .and_then(|u| u.get("extraRateWindows"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    extras
        .iter()
        .filter_map(|item| {
            let title = item.get("title").and_then(Value::as_str)?;
            let win = item.get("window")?;
            let used_percent = win.get("usedPercent")?.as_f64()?;
            let label = normalize_extra_label(title);
            if label.is_empty() {
                return None;
            }
            Some(UsageWindow {
                label,
                used_percent,
                resets_at_ms: win
                    .get("resetsAt")
                    .and_then(Value::as_str)
                    .and_then(pa_types::incident::timestamp_to_ms),
                reset_description: win
                    .get("resetDescription")
                    .and_then(Value::as_str)
                    .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|text| !text.is_empty()),
            })
        })
        .collect()
}

fn parse_provider(entry: &Value) -> Option<ProviderUsage> {
    let provider = entry.get("provider")?.as_str()?.to_string();
    let usage = entry.get("usage");
    let labels = entry.get("rateWindowLabels");
    let extra = parse_extra_windows(usage);
    let mut windows: Vec<UsageWindow> = if provider.eq_ignore_ascii_case("antigravity")
        && !extra.is_empty()
    {
        // Antigravity reports the real per-pool 5h/weekly quotas (Gemini vs Claude/GPT) in
        // `extraRateWindows`; primary/secondary only repeat the two 5h summaries.
        extra
    } else {
        let mut base: Vec<(UsageWindow, Option<String>)> = ["primary", "secondary", "tertiary"]
            .iter()
            .filter_map(|slot| {
                let rate_label = labels
                    .and_then(|labels| labels.get(slot))
                    .and_then(Value::as_str);
                let window = parse_window(usage?.get(slot)?, rate_label)?;
                Some((window, rate_label.map(str::to_lowercase)))
            })
            .collect();
        // Two windows of one length (cursor's monthly pools) read by the provider's own names.
        let shared: Vec<bool> = base
            .iter()
            .map(|(window, _)| {
                base.iter()
                    .filter(|(other, _)| other.label == window.label)
                    .count()
                    > 1
            })
            .collect();
        for ((window, name), shared) in base.iter_mut().zip(shared) {
            if let Some(name) = name.as_ref().filter(|_| shared) {
                window.label.clone_from(name);
            }
        }
        let mut combined: Vec<UsageWindow> = base.into_iter().map(|(window, _)| window).collect();
        for extra_window in extra {
            if !combined.iter().any(|w| w.label == extra_window.label) {
                combined.push(extra_window);
            }
        }
        combined
    };
    windows.retain(|w| !w.label.is_empty());
    Some(ProviderUsage {
        provider,
        source: entry
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string),
        windows,
        error: entry.get("error").and_then(error_text),
        cost: None,
    })
}

/// Parse `codexbar usage --json-only`: an array of provider objects.
///
/// # Errors
/// The output is not JSON, or not an array or object.
pub fn parse_usage(json: &str) -> Result<Vec<ProviderUsage>, String> {
    let value: Value =
        serde_json::from_str(json).map_err(|error| format!("unreadable usage output: {error}"))?;
    let entries = match &value {
        Value::Array(entries) => entries.as_slice(),
        Value::Object(_) => std::slice::from_ref(&value),
        _ => return Err("unexpected usage output".to_string()),
    };
    Ok(entries.iter().filter_map(parse_provider).collect())
}

/// Parse `codexbar cost --provider <name> --json-only`: one object, or a one-element array.
///
/// # Errors
/// The output is not JSON, is empty, or carries codexbar's `error` field.
pub fn parse_cost(json: &str) -> Result<ProviderCost, String> {
    let value: Value =
        serde_json::from_str(json).map_err(|error| format!("unreadable cost output: {error}"))?;
    let entry = match &value {
        Value::Array(entries) => entries.first().ok_or("empty cost output")?,
        Value::Object(_) => &value,
        _ => return Err("unexpected cost output".to_string()),
    };
    if let Some(error) = entry.get("error").and_then(error_text) {
        return Err(error);
    }
    let today_usd = entry.get("sessionCostUSD").and_then(Value::as_f64);
    let last_30_days_usd = entry
        .get("last30DaysCostUSD")
        .and_then(Value::as_f64)
        .or_else(|| entry.pointer("/totals/totalCost").and_then(Value::as_f64));
    // codexbar's "session" is its current day. The newest daily row is that day only when
    // its total matches; an older newest row (no spend today) carries no breakdown.
    let newest = entry
        .get("daily")
        .and_then(Value::as_array)
        .and_then(|daily| {
            daily.iter().max_by(|a, b| {
                let date =
                    |row: &Value| row.get("date").and_then(Value::as_str).map(str::to_string);
                date(a).cmp(&date(b))
            })
        });
    let today_models = match (newest, today_usd) {
        (Some(day), Some(today))
            if today > 0.0
                && day
                    .get("totalCost")
                    .and_then(Value::as_f64)
                    .is_some_and(|total| (total - today).abs() < 0.005) =>
        {
            let mut models: Vec<ModelSpend> = day
                .get("modelBreakdowns")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|model| {
                    Some(ModelSpend {
                        name: model.get("modelName")?.as_str()?.to_string(),
                        cost_usd: model.get("cost").and_then(Value::as_f64),
                        tokens: model
                            .get("totalTokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    })
                })
                .collect();
            models.sort_by(|a, b| {
                b.cost_usd
                    .unwrap_or(0.0)
                    .total_cmp(&a.cost_usd.unwrap_or(0.0))
                    .then(b.tokens.cmp(&a.tokens))
            });
            models
        }
        _ => Vec::new(),
    };
    Ok(ProviderCost {
        today_usd,
        last_30_days_usd,
        today_models,
    })
}

/// The `codexbar` binary: the env override, then `PATH`, then the install fallbacks.
#[must_use]
pub fn find_codexbar() -> Option<PathBuf> {
    let fallbacks: Vec<&Path> = CODEXBAR_FALLBACK_PATHS.iter().map(Path::new).collect();
    find_codexbar_in(
        std::env::var_os(CODEXBAR_BIN_ENV).as_deref(),
        std::env::var_os("PATH").as_deref(),
        &fallbacks,
    )
}

fn find_codexbar_in(
    override_bin: Option<&OsStr>,
    path: Option<&OsStr>,
    fallbacks: &[&Path],
) -> Option<PathBuf> {
    if let Some(bin) = override_bin.filter(|bin| !bin.is_empty()) {
        // An explicit override is authoritative: a wrong path reports as not found.
        let bin = PathBuf::from(bin);
        return bin.is_file().then_some(bin);
    }
    path.iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join("codexbar"))
        .chain(fallbacks.iter().map(|path| path.to_path_buf()))
        .find(|candidate| candidate.is_file())
}

/// Run codexbar and return its stdout. codexbar writes JSON errors to stdout with a non-zero
/// exit, so stdout is read whatever the status; a timeout kills the child.
async fn run_codexbar(bin: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut command = tokio::process::Command::new(bin);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(timeout, command.output()).await {
        Err(_) => return Err(format!("timed out after {}s", timeout.as_secs())),
        Ok(Err(error)) => return Err(format!("could not run codexbar: {error}")),
        Ok(Ok(output)) => output,
    };
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    Err(if stderr.is_empty() {
        format!("codexbar exited with {}", output.status)
    } else {
        stderr.trim().to_string()
    })
}

/// One step of a background load. The list arrives first; each provider's cost follows as
/// its own `codexbar cost` call settles, so a slow provider never holds back the others.
#[derive(Debug, Clone, PartialEq)]
pub enum UsageEvent {
    /// The provider list with its windows; every cost is pending (`None`).
    Listed(UsageReport),
    /// The cost of the provider at `index` in the listed report.
    Cost {
        index: usize,
        cost: Result<ProviderCost, String>,
    },
}

impl UsageReport {
    /// Providers whose cost fetch has not settled. Zero once the load is complete.
    #[must_use]
    pub fn pending_costs(&self) -> usize {
        match self {
            Self::Loaded(providers) => providers.iter().filter(|p| p.cost.is_none()).count(),
            Self::Unavailable | Self::Failed(_) => 0,
        }
    }

    /// Settle one provider's cost; an index outside the list is ignored.
    pub fn set_cost(&mut self, index: usize, cost: Result<ProviderCost, String>) {
        if let Self::Loaded(providers) = self {
            if let Some(provider) = providers.get_mut(index) {
                provider.cost = Some(cost);
            }
        }
    }
}

/// Load the report progressively: `emit` gets the usage list, then one event per provider
/// cost in completion order. Every listed provider gets exactly one cost event. A readable
/// usage list is also written to `quota_cache` (the router's `router-quota-cache.json`), so
/// the Captain and factory routers read the same fresh quota without their own `codexbar` run.
pub async fn load_usage(quota_cache: Option<PathBuf>, emit: impl FnMut(UsageEvent)) {
    load_usage_with(find_codexbar(), quota_cache, emit).await;
}

async fn load_usage_with(
    bin: Option<PathBuf>,
    quota_cache: Option<PathBuf>,
    mut emit: impl FnMut(UsageEvent),
) {
    let Some(bin) = bin else {
        emit(UsageEvent::Listed(UsageReport::Unavailable));
        return;
    };
    let providers = match run_codexbar(&bin, &["usage", "--json-only"], USAGE_TIMEOUT)
        .await
        .and_then(|json| parse_usage(&json).map(|providers| (json, providers)))
    {
        Ok((json, providers)) => {
            if let Some(path) = quota_cache {
                // The cache only saves the routers a `codexbar` run: a failed write leaves them
                // on their own refresh, so the panel goes on without it.
                let _ = tokio::task::spawn_blocking(move || write_quota_cache(&path, &json)).await;
            }
            providers
        }
        Err(error) => {
            emit(UsageEvent::Listed(UsageReport::Failed(error)));
            return;
        }
    };
    let names: Vec<String> = providers.iter().map(|p| p.provider.clone()).collect();
    emit(UsageEvent::Listed(UsageReport::Loaded(providers)));
    // Dropping the set (an aborted load) aborts the cost tasks, and their dropped commands
    // kill the codexbar children.
    let mut tasks = tokio::task::JoinSet::new();
    let mut indexes = std::collections::HashMap::new();
    for (index, name) in names.into_iter().enumerate() {
        let bin = bin.clone();
        let handle = tasks.spawn(async move {
            run_codexbar(
                &bin,
                &["cost", "--provider", &name, "--json-only"],
                COST_TIMEOUT,
            )
            .await
            .and_then(|json| parse_cost(&json))
        });
        indexes.insert(handle.id(), index);
    }
    while let Some(joined) = tasks.join_next_with_id().await {
        let (id, cost) = match joined {
            Ok((id, cost)) => (id, cost),
            Err(error) => (error.id(), Err(format!("cost task failed: {error}"))),
        };
        if let Some(&index) = indexes.get(&id) {
            emit(UsageEvent::Cost { index, cost });
        }
    }
}

/// Write `{"fetched_at": <unix secs>, "raw": <codexbar stdout>}`, the document
/// `load_quota_snapshot` in `model_router.py` reads, through a temporary file and a rename so a
/// concurrent reader never sees a partial document.
fn write_quota_cache(path: &Path, raw: &str) -> std::io::Result<()> {
    let fetched_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64());
    let body = serde_json::json!({ "fetched_at": fetched_at, "raw": raw }).to_string();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".{}.tmp", std::process::id()));
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, body)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// The outcome of one key press on the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageViewAction {
    None,
    Refresh,
    Close,
}

/// The `/usage` panel over the editor dock.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageView {
    /// `None` while the first load runs.
    report: Option<UsageReport>,
    fetched_at: Option<Instant>,
    refreshing: bool,
    /// This chat's spend from the tray, shown under the providers.
    session_cost_usd: Option<f64>,
    scroll: usize,
    /// The content window's height from the last paint (keys scroll the window the user saw).
    visible_rows: usize,
    /// The content's row count from the last paint.
    content_rows: usize,
}

impl UsageView {
    #[must_use]
    pub fn new(
        report: Option<UsageReport>,
        fetched_at: Option<Instant>,
        refreshing: bool,
        session_cost_usd: Option<f64>,
    ) -> Self {
        Self {
            report,
            fetched_at,
            refreshing,
            session_cost_usd,
            scroll: 0,
            visible_rows: 0,
            content_rows: 0,
        }
    }

    /// A background load settled.
    pub fn set_report(&mut self, report: UsageReport, fetched_at: Instant) {
        self.report = Some(report);
        self.fetched_at = Some(fetched_at);
        self.refreshing = false;
    }

    /// One provider's cost settled.
    pub fn set_cost(&mut self, index: usize, cost: Result<ProviderCost, String>) {
        if let Some(report) = self.report.as_mut() {
            report.set_cost(index, cost);
        }
    }

    pub fn set_refreshing(&mut self) {
        self.refreshing = true;
    }

    #[must_use]
    pub fn is_refreshing(&self) -> bool {
        self.refreshing
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.content_rows.saturating_sub(self.visible_rows.max(1));
        self.scroll = (self.scroll as isize)
            .saturating_add(delta)
            .clamp(0, max as isize) as usize;
    }

    pub fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> UsageViewAction {
        if key == "ctrl+c"
            || key == "q"
            || kb.matches(key, "tui.select.cancel")
            || kb.matches(key, "app.modal.back")
        {
            return UsageViewAction::Close;
        }
        if key == "r" {
            return UsageViewAction::Refresh;
        }
        let page = self.visible_rows.max(1) as isize;
        if key == "k" || kb.matches(key, "tui.select.up") {
            self.scroll_by(-1);
        } else if key == "j" || kb.matches(key, "tui.select.down") {
            self.scroll_by(1);
        } else if kb.matches(key, "tui.select.pageUp") {
            self.scroll_by(-page);
        } else if kb.matches(key, "tui.select.pageDown") {
            self.scroll_by(page);
        } else if kb.matches(key, "tui.select.top") {
            self.scroll = 0;
        } else if kb.matches(key, "tui.select.bottom") {
            self.scroll_by(isize::MAX);
        }
        UsageViewAction::None
    }

    /// Render at the current row budget; the frame never exceeds `viewport_rows`.
    pub fn render(
        &mut self,
        theme: &Theme,
        width: usize,
        kb: &KeybindingsManager,
        viewport_rows: usize,
    ) -> Vec<Line> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64);
        let age = self.fetched_at.map(|at| at.elapsed());
        self.render_at(theme, width, kb, viewport_rows, now_ms, age)
    }

    fn render_at(
        &mut self,
        theme: &Theme,
        width: usize,
        kb: &KeybindingsManager,
        viewport_rows: usize,
        now_ms: i64,
        age: Option<Duration>,
    ) -> Vec<Line> {
        let width = width.max(1);
        let viewport_rows = viewport_rows.max(1);
        let content = self.content_rows_at(theme, width, now_ms);
        let total = content.len();
        self.content_rows = total;
        let visible = menu_list_layout(
            Some(viewport_rows),
            viewport_rows,
            total,
            FIXED_FRAME_ROWS,
            SCROLL_INDICATOR_ROWS,
        );
        self.visible_rows = visible;
        self.scroll = self.scroll.min(total.saturating_sub(visible.max(1)));
        let end = (self.scroll + visible).min(total);
        let scrolled = end < total || self.scroll > 0;
        let mut lines: Vec<Line> = Vec::with_capacity(viewport_rows);
        lines.push(Vec::new());
        // Title row: crisp left heading + right-aligned freshness status.
        let status = self.status_text(age);
        let mut title: Line = vec![
            Span::raw("  "),
            theme.bold(theme.fg_span(ThemeColor::Text, "Usage & Quotas")),
        ];
        if let Some(status) = status {
            let left_w: usize = title.iter().map(|s| str_width(&s.content)).sum();
            let status_w = str_width(&status);
            let gap = if left_w + 2 + status_w < width {
                width - left_w - status_w - 2
            } else {
                2
            };
            title.push(Span::raw(" ".repeat(gap)));
            title.push(theme.fg_span(ThemeColor::Muted, status));
        }
        lines.push(truncate_line(&title, width, ""));
        let rule_w = width.saturating_sub(4).max(1);
        lines.push(vec![
            Span::raw(if width >= 4 { "  " } else { "" }),
            theme.fg_span(ThemeColor::BorderMuted, "\u{2500}".repeat(rule_w)),
        ]);
        lines.push(Vec::new());
        lines.extend(content[self.scroll..end].iter().cloned());
        if scrolled {
            lines.push(scroll_row(theme, width, self.scroll + 1, total));
        }
        // Pin the hint footer cleanly near the bottom of the full-page viewport.
        let footer_rows = 2;
        while lines.len() + footer_rows < viewport_rows {
            lines.push(Vec::new());
        }
        lines.push(hint_row(theme, width, &hint_text(kb)));
        lines.push(Vec::new());
        lines.truncate(viewport_rows);
        lines
    }

    fn status_text(&self, age: Option<Duration>) -> Option<String> {
        match (&self.report, self.refreshing) {
            (None, _) => Some("loading\u{2026}".to_string()),
            (Some(UsageReport::Unavailable), false) => Some("codexbar not found".to_string()),
            (Some(_), true) => Some("refreshing\u{2026}".to_string()),
            (Some(report), false) if report.pending_costs() > 0 => {
                Some("loading spend\u{2026}".to_string())
            }
            (Some(_), false) => age.map(|age| format!("updated {} ago", format_age(age))),
        }
    }

    fn summary_banner(&self, theme: &Theme, width: usize) -> Option<Line> {
        let mut today_sum = 0.0_f64;
        let mut month_sum = 0.0_f64;
        let mut has_today = false;
        let mut has_month = false;
        if let Some(UsageReport::Loaded(providers)) = &self.report {
            for p in providers {
                if let Some(Ok(cost)) = &p.cost {
                    if let Some(t) = cost.today_usd {
                        today_sum += t;
                        has_today = true;
                    }
                    if let Some(m) = cost.last_30_days_usd {
                        month_sum += m;
                        has_month = true;
                    }
                }
            }
        }
        let session = self.session_cost_usd.filter(|cost| *cost >= 0.0);
        if !has_today && !has_month && session.is_none() {
            return None;
        }
        let muted = theme.fg_style(ThemeColor::Muted);
        let value = theme
            .fg_style(ThemeColor::Text)
            .add_modifier(ratatui::style::Modifier::BOLD);
        let dim = theme.fg_style(ThemeColor::Dim);
        let mut spans: Vec<Span> = vec![Span::raw("  ")];
        let mut first = true;
        let push_sep = |spans: &mut Vec<Span>, first: &mut bool| {
            if !*first {
                spans.push(Span::styled("   \u{b7}   ", dim));
            }
            *first = false;
        };
        if has_today {
            push_sep(&mut spans, &mut first);
            spans.push(Span::styled("Today ", muted));
            spans.push(Span::styled(format_usd(today_sum), value));
        }
        if has_month {
            push_sep(&mut spans, &mut first);
            spans.push(Span::styled("30d ", muted));
            spans.push(Span::styled(format_usd(month_sum), value));
        }
        if let Some(cost) = session {
            push_sep(&mut spans, &mut first);
            spans.push(Span::styled("This chat ", muted));
            spans.push(Span::styled(format_usd(cost), value));
        }
        Some(truncate_line(&spans, width, "\u{2026}"))
    }

    fn content_rows_at(&self, theme: &Theme, width: usize, now_ms: i64) -> Vec<Line> {
        let mut rows: Vec<Line> = Vec::new();
        if let Some(banner) = self.summary_banner(theme, width) {
            rows.push(banner);
            rows.push(Vec::new());
        }
        match &self.report {
            None => rows.push(vec![
                Span::raw("  "),
                theme.fg_span(ThemeColor::Muted, "Fetching provider quotas\u{2026}"),
            ]),
            Some(UsageReport::Unavailable) => rows.push(vec![
                Span::raw("  "),
                theme.fg_span(
                    ThemeColor::Muted,
                    format!("codexbar not found. Install it or set {CODEXBAR_BIN_ENV}."),
                ),
            ]),
            Some(UsageReport::Failed(error)) => rows.push(vec![
                Span::raw("  "),
                theme.fg_span(ThemeColor::Error, format!("codexbar failed: {error}")),
            ]),
            Some(UsageReport::Loaded(providers)) if providers.is_empty() => rows.push(vec![
                Span::raw("  "),
                theme.fg_span(ThemeColor::Muted, "codexbar reports no providers"),
            ]),
            Some(UsageReport::Loaded(providers)) => {
                // Align window labels consistently across all providers for clean columns.
                let global_label_width = providers
                    .iter()
                    .flat_map(|p| p.windows.iter())
                    .map(|w| str_width(&w.label))
                    .max()
                    .unwrap_or(5)
                    .clamp(6, 18);
                for (index, provider) in providers.iter().enumerate() {
                    if index > 0 {
                        rows.push(Vec::new());
                    }
                    provider_rows(provider, global_label_width, theme, width, now_ms, &mut rows);
                }
            }
        }
        rows.into_iter()
            .map(|row| truncate_line(&row, width, "\u{2026}"))
            .collect()
    }
}

fn provider_display_name(raw: &str) -> String {
    match raw.to_ascii_lowercase().as_str() {
        "openai" => "OpenAI".to_string(),
        "zai" | "z.ai" => "Z.AI".to_string(),
        "xai" => "xAI".to_string(),
        "claude" => "Claude".to_string(),
        "codex" => "Codex".to_string(),
        "cursor" => "Cursor".to_string(),
        "antigravity" => "Antigravity".to_string(),
        "gemini" => "Gemini".to_string(),
        _ => {
            let mut chars = raw.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        }
    }
}

fn is_unsupported_cost_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("unknown provider")
        || lower.contains("unsupported provider")
        || lower.contains("not supported")
}

fn provider_status_color(provider: &ProviderUsage) -> ThemeColor {
    if provider.error.is_some() {
        return ThemeColor::Warning;
    }
    let worst = provider
        .windows
        .iter()
        .map(|w| w.used_percent)
        .fold(None, |acc: Option<f64>, p| {
            Some(acc.map_or(p, |cur| cur.max(p)))
        });
    match worst {
        Some(pct) => bar_color(pct),
        None => ThemeColor::Dim,
    }
}

fn hint_text(kb: &KeybindingsManager) -> String {
    let scroll = crate::menu_panel::key_hint(kb, &["tui.select.up", "tui.select.down"], "scroll");
    let close = kb.first_key("tui.select.cancel").map_or_else(
        || "Ctrl+C".to_string(),
        |key| crate::keybindings::format_key_text(&key),
    );
    match scroll {
        Some(scroll) => format!("r refresh \u{b7} {scroll} \u{b7} {close} close"),
        None => format!("r refresh \u{b7} {close} close"),
    }
}

/// One provider: status dot + capitalized title + spend, sleek bars per window, top models, error.
fn provider_rows(
    provider: &ProviderUsage,
    label_width: usize,
    theme: &Theme,
    width: usize,
    now_ms: i64,
    rows: &mut Vec<Line>,
) {
    let display_name = provider_display_name(&provider.provider);
    let mut header: Line = vec![
        Span::raw("  "),
        theme.fg_span(provider_status_color(provider), "\u{25cf}"),
        Span::raw(" "),
        theme.bold(theme.fg_span(ThemeColor::Text, display_name)),
    ];
    if let Some(source) = &provider.source {
        header.push(Span::raw("  "));
        header.push(theme.fg_span(ThemeColor::Dim, source.clone()));
    }
    let spend = match &provider.cost {
        Some(Ok(cost)) => spend_summary(cost).map(|spend| (spend, ThemeColor::Muted)),
        Some(Err(_)) => None,
        None => Some(("spend loading\u{2026}".to_string(), ThemeColor::Dim)),
    };
    if let Some((spend, color)) = spend {
        let used: usize = header.iter().map(|span| str_width(&span.content)).sum();
        let spend_width = str_width(&spend);
        let gap = if used + 2 + spend_width + 2 <= width {
            width - used - spend_width - 2
        } else {
            2
        };
        header.push(Span::raw(" ".repeat(gap)));
        header.push(theme.fg_span(color, spend));
    }
    rows.push(header);
    let bar_width = if width >= 68 {
        BAR_CELLS
    } else if width >= 48 {
        16
    } else {
        10
    };
    for window in &provider.windows {
        rows.push(window_row(window, label_width, bar_width, theme, width, now_ms));
    }
    if let Some(Ok(cost)) = &provider.cost {
        if let Some(models) = models_summary(&cost.today_models) {
            rows.push(vec![
                Span::raw("    "),
                theme.fg_span(ThemeColor::Dim, models),
            ]);
        }
    }
    if let Some(error) = &provider.error {
        rows.push(vec![
            Span::raw("    "),
            theme.fg_span(ThemeColor::Warning, error.clone()),
        ]);
    } else if let Some(Err(error)) = &provider.cost {
        if !is_unsupported_cost_error(error) {
            rows.push(vec![
                Span::raw("    "),
                theme.fg_span(ThemeColor::Dim, format!("spend unavailable: {error}")),
            ]);
        }
    }
}

fn spend_summary(cost: &ProviderCost) -> Option<String> {
    let parts: Vec<String> = [
        cost.today_usd
            .map(|usd| format!("today {}", format_usd(usd))),
        cost.last_30_days_usd
            .map(|usd| format!("30d {}", format_usd(usd))),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join(" \u{b7} "))
}

fn models_summary(models: &[ModelSpend]) -> Option<String> {
    let parts: Vec<String> = models
        .iter()
        .take(TOP_MODELS)
        .map(|model| match model.cost_usd {
            Some(cost) => format!(
                "{} {} \u{b7} {}",
                model.name,
                format_tokens(model.tokens),
                format_usd(cost)
            ),
            None => format!("{} {}", model.name, format_tokens(model.tokens)),
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("   "))
}

/// The bar color: success under 60%, warning under 85%, error from 85%.
#[must_use]
pub fn bar_color(used_percent: f64) -> ThemeColor {
    if used_percent >= 85.0 {
        ThemeColor::Error
    } else if used_percent >= 60.0 {
        ThemeColor::Warning
    } else {
        ThemeColor::Success
    }
}

/// `(filled, empty)` cell strings for a `cells`-wide sleek bar.
#[must_use]
pub fn bar_cells_width(used_percent: f64, cells: usize) -> (String, String) {
    let fraction = (used_percent / 100.0).clamp(0.0, 1.0);
    let filled = ((fraction * cells as f64).round() as usize).min(cells);
    let mut filled_str = String::with_capacity(filled * BAR_FILLED.len_utf8());
    for _ in 0..filled {
        filled_str.push(BAR_FILLED);
    }
    let mut empty_str = String::with_capacity((cells - filled) * BAR_EMPTY.len_utf8());
    for _ in 0..(cells - filled) {
        empty_str.push(BAR_EMPTY);
    }
    (filled_str, empty_str)
}

/// `(filled, empty)` cell strings for a `BAR_CELLS`-wide bar.
#[must_use]
pub fn bar_cells(used_percent: f64) -> (String, String) {
    bar_cells_width(used_percent, BAR_CELLS)
}

fn window_row(
    window: &UsageWindow,
    label_width: usize,
    bar_width: usize,
    theme: &Theme,
    width: usize,
    now_ms: i64,
) -> Line {
    let (filled, empty) = bar_cells_width(window.used_percent, bar_width);
    let label = truncate_to_width(&window.label, label_width, "\u{2026}");
    let pad = label_width.saturating_sub(str_width(&label));
    let color = bar_color(window.used_percent);
    let pct_text = format!("{:>3.0}%", window.used_percent.clamp(0.0, 999.0));
    let pct_span = if window.used_percent >= 85.0 {
        theme.bold(theme.fg_span(ThemeColor::Error, pct_text))
    } else if window.used_percent >= 60.0 {
        theme.fg_span(ThemeColor::Warning, pct_text)
    } else {
        theme.fg_span(ThemeColor::Text, pct_text)
    };
    let mut row: Line = vec![
        Span::raw("    "),
        theme.fg_span(ThemeColor::Muted, format!("{label}{}", " ".repeat(pad))),
        Span::raw("  "),
        theme.fg_span(color, filled),
        theme.fg_span(ThemeColor::BorderMuted, empty),
        Span::raw("  "),
        pct_span,
    ];
    let used: usize = row.iter().map(|span| str_width(&span.content)).sum();
    if let Some(reset) = reset_label(window, now_ms) {
        let room = width.saturating_sub(used + 2);
        if room >= 4 {
            row.push(Span::raw("  "));
            row.push(theme.fg_span(ThemeColor::Dim, truncate_to_width(&reset, room, "\u{2026}")));
        }
    }
    row
}

/// A compact countdown from `resetsAt`, else codexbar's own description.
fn reset_label(window: &UsageWindow, now_ms: i64) -> Option<String> {
    if let Some(at) = window.resets_at_ms {
        let left = at - now_ms;
        if left <= 0 {
            return Some("\u{21bb} reset due".to_string());
        }
        return Some(format!(
            "\u{21bb} {}",
            format_countdown(Duration::from_millis(left as u64))
        ));
    }
    window.reset_description.clone()
}

/// `3d 4h`, `3h 46m`, `12m`, or `<1m`.
fn format_countdown(left: Duration) -> String {
    let minutes = left.as_secs() / 60;
    let (days, hours, mins) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        "<1m".to_string()
    }
}

fn format_age(age: Duration) -> String {
    let secs = age.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// `$1,220.94`.
fn format_usd(usd: f64) -> String {
    let cents = (usd.max(0.0) * 100.0).round() as u64;
    let dollars = (cents / 100).to_string();
    let mut grouped = String::new();
    for (index, digit) in dollars.chars().enumerate() {
        if index > 0 && (dollars.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("${grouped}.{:02}", cents % 100)
}

/// `950`, `12.3k`, `63.1M`, `2.5B`.
fn format_tokens(tokens: u64) -> String {
    let value = tokens as f64;
    if tokens >= 1_000_000_000 {
        format!("{:.1}B", value / 1e9)
    } else if tokens >= 1_000_000 {
        format!("{:.1}M", value / 1e6)
    } else if tokens >= 1_000 {
        format!("{:.1}k", value / 1e3)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests;
