//! The Captain panel: every project Captain on this machine, the selected project's records,
//! and its crew (threads and factory runs). F6 and `/captain` open it.
//!
//! The view is presentation and selection only. The Python Captain backend owns the data and
//! every action: the session UI reads `pa thread captain status` and runs
//! `pa thread captain act OP` for each [`CaptainAction::Act`] the view returns. Text input
//! (a brief, a message, an answer) uses pa's own prompt editor through
//! [`CaptainAction::Compose`]; lists use the panel's picker.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::keybindings::KeybindingsManager;
use crate::menu_panel::{menu_row, rule_row, MenuSegment};
use crate::theme::{Theme, ThemeColor};
use crate::width::{line_width, str_width, truncate_line, truncate_to_width};
use crate::{Line, Span};

#[cfg(test)]
mod tests;

/// `pa thread captain status`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct CaptainSnapshot {
    pub current_root: String,
    pub current: Option<String>,
    pub projects: Vec<CaptainProject>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct CaptainProject {
    pub id: String,
    pub root: String,
    pub name: String,
    pub objective: String,
    pub rules: String,
    pub plan: String,
    pub crew_rules: String,
    pub digest: String,
    pub provider: String,
    pub model: String,
    pub thinking: String,
    pub parallel: i64,
    /// active, stopping, paused, or deleting.
    pub state: String,
    pub lead: String,
    pub lead_state: String,
    pub lead_detail: Option<String>,
    pub lead_session: Option<String>,
    pub lead_busy: bool,
    pub crew: Vec<CaptainMember>,
    pub edits: Vec<CaptainEdit>,
    /// Machines the Captain wrote in `<root>/.prime/machines/<name>/`.
    pub factories: Vec<ProjectFactory>,
    pub factories_error: Option<String>,
    /// Project specialists from `<root>/.prime/agents/<id>.md`.
    pub specialists: Vec<ProjectSpecialist>,
    /// Every human gate of the project, from the backend's `needs_you_list`; none from a
    /// backend that predates it.
    pub needs_you: Option<Vec<NeedsYouItem>>,
    pub unread: bool,
    pub unread_count: i64,
    /// Recent user notifications, newest first.
    pub notifications: Vec<CaptainNotification>,
    /// The unread ones, from a backend that lists only those.
    pub unread_notifications: Vec<CaptainNotification>,
    pub usage: Usage,
    pub budget: Budget,
    /// Per-event wake policy (`react`, `context`, or `ignore`) and `debounce_seconds`.
    pub wake_policy: serde_json::Map<String, Value>,
    pub schedules: Vec<CaptainSchedule>,
    pub decisions: Vec<Decision>,
    pub drive_files: DriveListing,
}

/// Token and dollar spend of a project or crew member.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct Budget {
    pub budget_usd: Option<f64>,
    pub budget_tokens: Option<u64>,
    pub spent_usd: f64,
    pub spent_tokens: u64,
    pub exceeded: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct CaptainSchedule {
    pub id: String,
    pub title: String,
    pub cron_expr: String,
    pub task: String,
    pub enabled: bool,
    pub last_run_at: Option<Value>,
    pub next_run_at: Option<Value>,
}

/// A structured escalation: the Captain's options for the user, each answered by one key.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct Decision {
    pub id: String,
    pub member: String,
    pub title: String,
    pub summary: String,
    pub recommendation: String,
    pub options: Vec<DecisionOption>,
    pub status: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct DecisionOption {
    pub key: String,
    pub label: String,
    pub reply_text: String,
}

/// Saved files under `.prime/captain/reports/` and `.prime/captain/handoffs/`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DriveListing {
    pub reports: Vec<String>,
    pub handoffs: Vec<String>,
}

impl<'de> serde::Deserialize<'de> for DriveListing {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Obj {
                #[serde(default)]
                reports: Vec<String>,
                #[serde(default)]
                handoffs: Vec<String>,
            },
            List(Vec<String>),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Obj { reports, handoffs } => Ok(Self { reports, handoffs }),
            Raw::List(items) => {
                let mut listing = Self::default();
                for item in items {
                    if let Some(name) = item.strip_prefix("reports/") {
                        listing.reports.push(name.to_owned());
                    } else if let Some(name) = item.strip_prefix("handoffs/") {
                        listing.handoffs.push(name.to_owned());
                    }
                }
                Ok(listing)
            }
        }
    }
}

/// A crew branch's pull request and its CI and review state.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct PrStatus {
    pub state: String,
    pub checks: String,
    pub review_decision: String,
}

/// `84k` for a token count.
fn tokens_label(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1000..=999_999 => format!("{}k", tokens / 1000),
        _ => format!("{:.1}M", tokens as f64 / 1_000_000.0),
    }
}

/// The wake-policy events, in drawer order.
const WAKE_EVENTS: [&str; 6] = [
    "done",
    "failed",
    "needs_you",
    "interrupted",
    "factory_done",
    "factory_paused",
];

fn next_wake_policy(policy: &str) -> &'static str {
    match policy {
        "react" => "context",
        "context" => "ignore",
        _ => "react",
    }
}

/// A value the backend sends as a string or a number, as text.
fn value_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => clean(s),
        Some(Value::Number(n)) => n
            .as_f64()
            .map(|at| age_label(at - now_seconds()))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// One human gate: a paused run, a factory draft, a writer-machine grant, a blocked crew
/// thread, a missing worktree, or the Captain's own question.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct NeedsYouItem {
    /// captain, paused_run, draft_factory, changed_factory, pending_grant, blocked_crew, or
    /// worktree_missing.
    pub kind: String,
    pub id: String,
    pub member: Option<String>,
    pub label: String,
    pub state: String,
    pub detail: String,
    pub step: Option<String>,
    pub factory: Option<String>,
    pub digest: String,
    pub grant_id: Option<String>,
    pub machine: Option<String>,
    pub decision_id: Option<String>,
    pub title: String,
    pub summary: String,
    pub recommendation: String,
    pub options: Vec<DecisionOption>,
}

impl NeedsYouItem {
    /// The keys this item answers to, for its row and the footer.
    fn hint(&self) -> &'static str {
        match self.kind.as_str() {
            "decision" => "1-6 pick · m custom reply",
            "budget_exceeded" => "b +$5 budget · c clear cap",
            "draft_factory" | "changed_factory" => "Enter review · a approve",
            "pending_grant" => "a approve once · g grant · x decline",
            "paused_run" => "Enter inspect · r retry · m answer · s stop",
            "blocked_crew" | "captain" => "Enter open · m reply",
            "worktree_missing" => "r recreate",
            _ => "",
        }
    }
}

/// A project specialist agent: its frontmatter and prompt (capped by the backend).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(default)]
pub(crate) struct ProjectSpecialist {
    pub id: String,
    pub name: String,
    pub description: String,
    pub model: String,
    pub thinking: String,
    pub prompt: String,
    pub path: String,
}

/// The crew states and factory states the last status showed, keyed by member id and by
/// `project/name`, so the next status can tell what changed.
pub(crate) type CrewStates = std::collections::HashMap<String, String>;

/// The toasts for crew members and project factories whose state changed into one that
/// needs the user or ended, and the new state map. `previous` none (the first status) only
/// records the states.
pub(crate) fn crew_toasts(
    previous: Option<&CrewStates>,
    snapshot: &CaptainSnapshot,
) -> (CrewStates, Vec<String>) {
    let mut states = CrewStates::new();
    let mut toasts = Vec::new();
    for project in &snapshot.projects {
        for member in &project.crew {
            let before = previous.and_then(|p| p.get(&member.id));
            if previous.is_some() && before != Some(&member.state) {
                let text = match member.state.as_str() {
                    "needs_you" => Some("needs you"),
                    "done" => Some("done"),
                    "failed" => Some("failed"),
                    _ => None,
                };
                if let Some(text) = text {
                    toasts.push(format!("{} · {} {text}", project.name, member.label()));
                }
            }
            states.insert(member.id.clone(), member.state.clone());
        }
        for factory in &project.factories {
            let key = format!("{}/{}", project.id, factory.name);
            let before = previous.and_then(|p| p.get(&key));
            if previous.is_some() && factory.needs_review() && before != Some(&factory.state) {
                toasts.push(format!(
                    "{} · factory {} needs review",
                    project.name, factory.name
                ));
            }
            states.insert(key, factory.state.clone());
        }
    }
    (states, toasts)
}

/// A project factory and its approval state, from the backend's `captain_factories.py`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct ProjectFactory {
    pub name: String,
    /// missing, invalid, draft, approved, or changed (since approval).
    pub state: String,
    pub path: String,
    pub file: String,
    pub digest: String,
    /// Whether the user approved some version of it.
    pub approved: bool,
    pub errors: Vec<String>,
    /// "specialist missing - write it again", "specialist changed - update", "module update
    /// available", or empty.
    pub drift: String,
}

impl ProjectFactory {
    /// A valid version that waits for the user's approval.
    fn needs_review(&self) -> bool {
        matches!(self.state.as_str(), "draft" | "changed")
    }
}

/// The short label of a project factory state.
fn factory_state_label(state: &str, approved: bool) -> &'static str {
    match state {
        "missing" => "missing",
        "invalid" if approved => "invalid · changed since approval",
        "invalid" => "invalid draft",
        "draft" => "draft · needs approval",
        "approved" => "approved",
        "changed" => "changed since approval · needs approval",
        _ => "unknown",
    }
}

/// One specialist as a picker or panel label: id, then model and thinking when set.
fn specialist_label(specialist: &ProjectSpecialist) -> String {
    let mut label = clean(&specialist.id);
    let model = specialist
        .model
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    for part in [model, specialist.thinking.clone()] {
        if !part.is_empty() {
            label.push_str(" · ");
            label.push_str(&clean(&part));
        }
    }
    label
}

/// The review-page rows for vendored modules and specialists, with their drift state.
fn vendored_rows(
    theme: &Theme,
    width: usize,
    modules: &[Value],
    specialists: &[Value],
) -> Vec<Line> {
    let text = |row: &Value, key: &str| row[key].as_str().unwrap_or_default().to_owned();
    let mut out = Vec::new();
    for row in modules {
        let version = row["version"].as_i64().unwrap_or(0);
        let (state, color) = match text(row, "state").as_str() {
            "update" => (
                format!("update to v{} - u", row["latest"].as_i64().unwrap_or(0)),
                ThemeColor::Warning,
            ),
            "missing" => ("not in the library".to_owned(), ThemeColor::Error),
            _ => ("current".to_owned(), ThemeColor::Dim),
        };
        out.push(truncate_line(
            &vec![
                Span::raw(" "),
                theme.fg_span(ThemeColor::Dim, "module "),
                theme.fg_span(ThemeColor::Text, clean(&text(row, "alias"))),
                theme.fg_span(
                    ThemeColor::Dim,
                    format!("  {} v{version} · ", clean(&text(row, "module"))),
                ),
                theme.fg_span(color, state),
            ],
            width,
            "…",
        ));
    }
    for row in specialists {
        let states = row["states"]
            .as_array()
            .map(|s| {
                s.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let (state, color) = match text(row, "state").as_str() {
            "changed" => ("specialist changed - update - u", ThemeColor::Warning),
            "missing" => ("specialist missing - write it again", ThemeColor::Error),
            _ => ("current", ThemeColor::Dim),
        };
        out.push(truncate_line(
            &vec![
                Span::raw(" "),
                theme.fg_span(ThemeColor::Dim, "specialist "),
                theme.fg_span(ThemeColor::Text, clean(&text(row, "id"))),
                theme.fg_span(ThemeColor::Dim, format!("  {} · ", clean(&states))),
                theme.fg_span(color, state),
            ],
            width,
            "…",
        ));
    }
    out
}

/// The update op for the first vendored part that drifted, as `(op, args key, value)`.
fn drift_update(review: &FactoryReview) -> Option<(&'static str, &'static str, String)> {
    let text = |row: &Value, key: &str| row[key].as_str().unwrap_or_default().to_owned();
    review
        .specialists
        .iter()
        .find(|row| text(row, "state") == "changed")
        .map(|row| ("factory-update-specialist", "specialist", text(row, "id")))
        .or_else(|| {
            review
                .modules
                .iter()
                .find(|row| text(row, "state") == "update")
                .map(|row| ("factory-update-module", "alias", text(row, "alias")))
        })
}

fn factory_glyph(state: &str) -> (&'static str, ThemeColor) {
    match state {
        "approved" => ("✓", ThemeColor::Success),
        "draft" | "changed" => ("!", ThemeColor::Warning),
        "invalid" => ("✗", ThemeColor::Error),
        _ => ("○", ThemeColor::Dim),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct CaptainMember {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub provider: String,
    pub model: String,
    pub thinking: String,
    pub state: String,
    pub detail: String,
    pub run_state: Option<String>,
    pub machine: Option<String>,
    pub progress: Option<String>,
    pub step: Option<String>,
    pub headless: bool,
    pub session: Option<String>,
    pub busy: bool,
    /// working, waiting, failed, or idle, apart from whether it needs the user.
    pub activity_state: String,
    pub needs_you: bool,
    pub archived: bool,
    pub worktree_missing: bool,
    pub diff_added: Option<i64>,
    pub diff_deleted: Option<i64>,
    pub updated_at: Option<f64>,
    /// A Captain run's native host session and its run id there.
    pub host_session: Option<String>,
    pub run_id: Option<String>,
    /// The human name, such as `eigen-pipeline · Phase 2 classical baselines`.
    pub display_name: String,
    pub unread: bool,
    pub unread_count: i64,
    pub steerable: bool,
    /// A run's states in flight.
    pub active_states: Vec<String>,
    /// A run's step subagents: the working ones first, then the waiting, then the settled.
    pub step_agents: Vec<StepAgent>,
    pub usage: Usage,
    /// The member whose branch this one stacks on.
    pub depends_on: String,
    /// The place in its stack, such as `2/3`.
    pub stack_position: String,
    pub pr_url: String,
    pub pr_number: Option<u64>,
    pub pr_status: Option<PrStatus>,
    pub open_findings_count: usize,
}

/// One subagent a factory run's state spawned (`step_agents` of a run row).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct StepAgent {
    pub state: String,
    pub role: String,
    pub model: String,
    pub thinking: String,
    /// running, queued, waiting, paused, error, failed, or done.
    pub status: String,
    pub child_id: String,
    pub child_name: String,
    pub session: String,
    pub session_id: String,
    pub attempt: Option<i64>,
    pub duration_ms: Option<f64>,
    pub progress_note: Option<String>,
    pub is_streaming: bool,
}

/// The most step rows one run shows under it.
const STEPS_SHOWN: usize = 6;

impl StepAgent {
    /// What `act open` takes after `step:`: the live session, else the child id.
    fn target(&self) -> &str {
        if self.session.is_empty() {
            &self.child_id
        } else {
            &self.session
        }
    }

    fn is_live(&self) -> bool {
        matches!(
            self.status.as_str(),
            "running" | "queued" | "waiting" | "paused" | "error" | "failed"
        )
    }

    fn glyph(&self) -> (&'static str, ThemeColor) {
        match self.status.as_str() {
            "running" => ("◐", ThemeColor::Accent),
            "queued" | "waiting" | "paused" => ("◐", ThemeColor::Warning),
            "error" | "failed" => ("✗", ThemeColor::Error),
            "done" => ("✓", ThemeColor::Success),
            _ => ("·", ThemeColor::Dim),
        }
    }

    /// `implement · gemini-4-argon · high · running`; a settled step only names its status.
    fn summary(&self) -> String {
        let mut parts = vec![clean(&self.state)];
        if self.is_live() {
            let model = self.model.trim();
            if !model.is_empty() {
                parts.push(
                    model
                        .rsplit_once('/')
                        .map_or(model, |(_, id)| id)
                        .to_owned(),
                );
            }
            if !self.thinking.trim().is_empty() {
                parts.push(clean(&self.thinking));
            }
        }
        if !self.status.is_empty() {
            parts.push(self.status.clone());
        }
        parts.join(" · ")
    }
}

/// One user notification of a project (`notifications` of a project row).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct CaptainNotification {
    pub id: Value,
    pub member: String,
    pub kind: String,
    pub title: String,
    pub detail: String,
    /// An ISO timestamp or epoch seconds.
    pub created_at: Value,
    pub read_at: String,
    pub read: bool,
}

impl CaptainNotification {
    fn is_unread(&self) -> bool {
        !self.read && self.read_at.is_empty()
    }

    fn age(&self) -> Option<String> {
        let at = match &self.created_at {
            Value::Number(n) => n.as_f64()?,
            Value::String(text) => text.parse::<f64>().ok().or_else(|| iso_seconds(text))?,
            _ => return None,
        };
        Some(age_label(now_seconds() - at))
    }
}

/// Epoch seconds of an ISO 8601 time such as `2026-11-23T06:08:00.12+00:00` (the backend's
/// `iso_now`); a missing offset reads as UTC.
fn iso_seconds(text: &str) -> Option<f64> {
    let num = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let tail = &text[19..];
    let offset = tail
        .find(['+', '-'])
        .and_then(|at| {
            let sign = if tail[at..].starts_with('-') { -1 } else { 1 };
            let zone = &tail[at + 1..];
            let hours = zone.get(0..2)?.parse::<i64>().ok()?;
            let minutes = zone
                .get(3..5)
                .and_then(|m| m.parse::<i64>().ok())
                .unwrap_or(0);
            Some(sign * (hours * 3600 + minutes * 60))
        })
        .unwrap_or(0);
    Some((days * 86_400 + hour * 3600 + minute * 60 + second - offset) as f64)
}

/// Whether `text` is only a member or session id such as `d0e6edff`, never a label.
fn is_bare_id(text: &str) -> bool {
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
    let parts: Vec<&str> = text.trim().split('-').collect();
    match parts.as_slice() {
        [one] => hex(one, 8),
        [low, mid, high, seq, node] => {
            hex(low, 8) && hex(mid, 4) && hex(high, 4) && hex(seq, 4) && hex(node, 12)
        }
        _ => false,
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct CaptainEdit {
    pub field: String,
    pub actor: String,
}

/// `pa thread captain act choices`: the options the forms offer.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct CaptainChoices {
    pub models: Vec<ModelChoice>,
    pub crew_models: Vec<CrewModel>,
    pub thinking: Vec<String>,
    pub machines: Vec<String>,
    pub machine_params: std::collections::BTreeMap<String, Vec<MachineParam>>,
    /// Approved project factories of the project the options are for.
    pub project_factories: Vec<String>,
    pub adopt: Vec<AdoptChoice>,
    pub brief: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct MachineParam {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub required: bool,
    pub description: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct ModelChoice {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct CrewModel {
    pub label: String,
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct AdoptChoice {
    #[serde(rename = "ref")]
    pub reference: String,
    pub label: String,
}

impl CaptainProject {
    pub(crate) fn display_state(&self) -> &str {
        match self.state.as_str() {
            "" => "active",
            other => other,
        }
    }

    fn is_active(&self) -> bool {
        self.display_state() == "active"
    }

    fn counts(&self) -> (usize, usize, usize) {
        let needs = self.needs_items().len();
        let mut running = usize::from(self.lead_state == "running");
        let mut done = 0;
        for member in self.crew.iter().filter(|m| !m.archived) {
            match member.state.as_str() {
                "running" => running += 1,
                "done" => done += 1,
                _ => {}
            }
        }
        (needs, running, done)
    }

    /// Every human gate, in the backend's order; an older backend's snapshot derives them
    /// from the crew and factory states.
    pub(crate) fn needs_items(&self) -> Vec<NeedsYouItem> {
        if let Some(items) = &self.needs_you {
            return items.clone();
        }
        let mut items = Vec::new();
        if self.lead_state == "needs_you" {
            items.push(NeedsYouItem {
                kind: "captain".into(),
                id: self.lead.clone(),
                label: "captain".into(),
                ..NeedsYouItem::default()
            });
        }
        for m in &self.crew {
            let kind = if m.worktree_missing {
                "worktree_missing"
            } else if m.is_run()
                && (m.run_state.as_deref() == Some("paused")
                    || matches!(m.state.as_str(), "needs_you" | "failed" | "unknown"))
            {
                "paused_run"
            } else if !m.is_run() && matches!(m.state.as_str(), "needs_you" | "failed" | "unknown")
            {
                "blocked_crew"
            } else {
                continue;
            };
            items.push(NeedsYouItem {
                kind: kind.into(),
                id: m.id.clone(),
                member: Some(m.id.clone()),
                label: m.label(),
                state: m.state.clone(),
                detail: m.detail.clone(),
                step: m.step.clone(),
                ..NeedsYouItem::default()
            });
        }
        for f in &self.factories {
            let kind = if f.needs_review() {
                format!("{}_factory", f.state)
            } else if f.drift.starts_with("specialist") {
                "changed_factory".into()
            } else {
                continue;
            };
            items.push(NeedsYouItem {
                kind,
                id: format!("{}/{}", self.id, f.name),
                label: format!("factory {}", f.name),
                detail: f.drift.clone(),
                factory: Some(f.name.clone()),
                digest: f.digest.clone(),
                ..NeedsYouItem::default()
            });
        }
        items
    }

    /// `$0.42 / $5.00` against a cap, `$0.42 · 84k tok` uncapped; none before any spend.
    fn spend_label(&self) -> Option<String> {
        let tokens = self.usage.input_tokens + self.usage.output_tokens;
        let spent = self.usage.cost_usd.max(self.budget.spent_usd);
        if spent <= 0.0 && tokens == 0 && self.budget.budget_usd.is_none() {
            return None;
        }
        Some(match self.budget.budget_usd {
            Some(cap) => format!("${spent:.2} / ${cap:.2}"),
            None => format!("${spent:.2} · {} tok", tokens_label(tokens)),
        })
    }

    /// One state for the rail's ■ indicator, in priority order.
    fn summary_state(&self) -> &'static str {
        match self.display_state() {
            "paused" => return "paused",
            "stopping" | "deleting" => return "stopping",
            _ => {}
        }
        let (needs, running, _) = self.counts();
        if needs > 0 {
            "needs you"
        } else if running > 0 {
            "running"
        } else {
            "idle"
        }
    }

    pub(crate) fn model_label(&self) -> String {
        let provider = if self.provider.is_empty() {
            "prime"
        } else {
            &self.provider
        };
        let model = if self.model.is_empty() {
            "default model"
        } else {
            &self.model
        };
        if self.thinking.is_empty() {
            format!("{provider} · {model}")
        } else {
            format!("{provider} · {model} · {}", self.thinking)
        }
    }

    /// The crew by activity: working, failed, idle, done, then the archived rows when shown.
    fn ordered_crew(&self, show_archived: bool) -> Vec<(&'static str, Vec<&CaptainMember>)> {
        let live = |m: &&CaptainMember| !m.archived;
        let group = |pick: &dyn Fn(&CaptainMember) -> bool| -> Vec<&CaptainMember> {
            self.crew.iter().filter(live).filter(|m| pick(m)).collect()
        };
        let mut groups = vec![
            (
                "working",
                group(&|m| matches!(m.activity(), "working" | "waiting")),
            ),
            ("failed", group(&|m| m.activity() == "failed")),
            (
                "idle",
                group(&|m| m.activity() == "idle" && m.state != "done"),
            ),
            (
                "done",
                group(&|m| m.activity() == "idle" && m.state == "done"),
            ),
        ];
        if show_archived {
            groups.push((
                "archived",
                self.crew.iter().filter(|m| m.archived).collect(),
            ));
        }
        groups
    }

    fn flat_crew(&self, show_archived: bool) -> Vec<&CaptainMember> {
        self.ordered_crew(show_archived)
            .into_iter()
            .flat_map(|(_, members)| members)
            .collect()
    }

    fn archived_count(&self) -> usize {
        self.crew.iter().filter(|m| m.archived).count()
    }

    /// The Inbox: the listed notifications, else the unread ones.
    fn inbox(&self) -> &[CaptainNotification] {
        if self.notifications.is_empty() {
            &self.unread_notifications
        } else {
            &self.notifications
        }
    }

    /// Unread crew updates and notifications, for the Overview header badge.
    fn unread_total(&self) -> usize {
        let notes = self.inbox().iter().filter(|n| n.is_unread()).count();
        usize::try_from(self.unread_count).unwrap_or(0).max(notes)
    }
}

impl CaptainMember {
    /// The short state text under a crew row.
    pub(crate) fn state_text(&self) -> String {
        let run = self.run_state.as_deref().unwrap_or("");
        match self.state.as_str() {
            "needs_you" => "needs you".into(),
            "running" => "working".into(),
            "ready" if self.busy => "working".into(),
            "ready" => "idle".into(),
            "done" if matches!(run, "stopped" | "cancelled") => "stopped".into(),
            "done" => "done".into(),
            "failed" => "failed".into(),
            _ if run == "paused" => "paused".into(),
            "" | "unknown" => "unknown".into(),
            other => other.replace('_', " "),
        }
    }

    /// The human name: the backend's display name, else the machine or title; never a bare
    /// member id.
    pub(crate) fn label(&self) -> String {
        let name = clean(&self.display_name);
        if !name.is_empty() && !is_bare_id(&name) {
            return name;
        }
        let label = if self.kind == "factory" {
            self.machine.clone().unwrap_or_else(|| clean(&self.title))
        } else {
            clean(&self.title)
        };
        if !is_bare_id(&label) && !label.is_empty() {
            label
        } else if self.is_run() {
            "factory run".into()
        } else {
            "crew thread".into()
        }
    }

    /// A live run's states in flight, as `▶ implement` or `▶ correctness + bugs`.
    pub(crate) fn stage_badge(&self) -> Option<String> {
        if !self.is_run() || !matches!(self.activity(), "working" | "waiting") {
            return None;
        }
        let stages = if self.active_states.is_empty() {
            self.step.clone().filter(|s| !s.is_empty())?
        } else {
            self.active_states.join(" + ")
        };
        Some(format!("▶ {}", clean(&stages)))
    }

    /// The step rows under a run: every live step, then the latest settled one.
    pub(crate) fn shown_steps(&self) -> Vec<&StepAgent> {
        if !self.is_run() || self.archived {
            return Vec::new();
        }
        let mut steps: Vec<&StepAgent> = self.step_agents.iter().filter(|s| s.is_live()).collect();
        steps.extend(self.step_agents.iter().find(|s| !s.is_live()));
        steps.truncate(STEPS_SHOWN);
        steps
    }

    pub(crate) fn is_run(&self) -> bool {
        self.kind == "factory"
    }

    /// Whether a run settled: done, failed, stopped, lost, or cancelled.
    fn run_ended(&self) -> bool {
        matches!(
            self.run_state.as_deref(),
            Some("done" | "failed" | "stopped" | "lost" | "cancelled")
        )
    }

    /// working, waiting, failed, or idle; derived from the state for an older backend.
    pub(crate) fn activity(&self) -> &str {
        if !self.activity_state.is_empty() {
            return &self.activity_state;
        }
        let run = self.run_state.as_deref().unwrap_or("");
        if self.state == "failed" || matches!(run, "failed" | "lost") {
            "failed"
        } else if run == "paused" || self.state == "needs_you" {
            "waiting"
        } else if self.busy
            || self.state == "running"
            || matches!(run, "queued" | "starting" | "running" | "stopping")
        {
            "working"
        } else {
            "idle"
        }
    }

    /// Whether the member waits on the user, apart from its activity.
    pub(crate) fn needs_user(&self) -> bool {
        self.needs_you
            || self.worktree_missing
            || matches!(self.state.as_str(), "needs_you" | "unknown")
            || self.run_state.as_deref() == Some("paused")
    }

    /// The F7 status: needs_you, working, paused, failed, or idle.
    pub(crate) fn chat_status(&self) -> &'static str {
        match self.activity() {
            "failed" => "failed",
            "waiting" if self.is_run() => "paused",
            _ if self.needs_user() => "needs_you",
            "working" => "working",
            "waiting" => "paused",
            _ => "idle",
        }
    }

    /// `+A -D` of a writer's worktree, when the backend measured it.
    fn diff_label(&self) -> Option<String> {
        match (self.diff_added, self.diff_deleted) {
            (Some(added), Some(deleted)) if added > 0 || deleted > 0 => {
                Some(format!("+{added} -{deleted}"))
            }
            _ => None,
        }
    }

    /// Short model badge for crew rows (e.g. `claude-sonnet-4-6 · high`).
    /// `[2/3]` in a stack, `↳ waits` on a parent without a position.
    fn stack_badge(&self) -> Option<String> {
        if !self.stack_position.is_empty() {
            Some(format!("[{}]", clean(&self.stack_position)))
        } else if !self.depends_on.is_empty() {
            Some("↳ waits".into())
        } else {
            None
        }
    }

    /// `PR #42 ✓` with its CI color: passing, failing, or pending.
    fn pr_pill(&self) -> Option<(String, ThemeColor)> {
        let number = self.pr_number?;
        let status = self.pr_status.clone().unwrap_or_default();
        let (glyph, color) = match (status.checks.as_str(), status.state.as_str()) {
            (_, "merged") => ("✓", ThemeColor::Success),
            ("success" | "passing" | "pass", _) => ("✓", ThemeColor::Success),
            ("failure" | "failing" | "fail" | "error", _) => ("✗", ThemeColor::Error),
            _ => ("◐", ThemeColor::Warning),
        };
        Some((format!("PR #{number} {glyph}"), color))
    }

    pub(crate) fn short_model_badge(&self) -> Option<String> {
        let raw = self.model.trim();
        if raw.is_empty() {
            return None;
        }
        let short = raw.rsplit_once('/').map_or(raw, |(_, id)| id);
        let thinking = self.thinking.trim();
        if thinking.is_empty() {
            Some(short.to_owned())
        } else {
            Some(format!("{short} · {thinking}"))
        }
    }
}

/// A target pa opens from the panel, as returned by the backend's `open` action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenTarget {
    /// A native session: pa's chat view opens it in place.
    Session {
        path: String,
        factory: bool,
        draft: Option<String>,
    },
    /// A provider CLI thread with no native session: the read-and-reply view.
    Thread { id: String, title: String },
    /// The factory page of the current chat.
    Factory,
    /// A draft for the current chat.
    Draft(String),
}

impl OpenTarget {
    /// The backend's `{"handoff": {...}}` reply.
    pub(crate) fn from_reply(reply: &Value) -> Option<Self> {
        let target = reply.get("handoff")?;
        let text = |key: &str| target.get(key).and_then(Value::as_str).map(str::to_owned);
        match target.get("action")?.as_str()? {
            "resume" => Some(Self::Session {
                path: text("value")?,
                factory: text("panel").as_deref() == Some("factory"),
                draft: text("draft").filter(|d| !d.trim().is_empty()),
            }),
            "thread" => Some(Self::Thread {
                id: text("value")?,
                title: text("title").unwrap_or_default(),
            }),
            "factory" => Some(Self::Factory),
            "draft" => Some(Self::Draft(text("value")?)),
            _ => None,
        }
    }

    /// The blank chat of a `new-thread` reply, opened in pa's chat view.
    pub(crate) fn from_new_thread(reply: &Value) -> Option<Self> {
        Self::from_reply(reply).or_else(|| {
            Some(Self::Session {
                path: reply.get("session")?.as_str()?.to_owned(),
                factory: false,
                draft: None,
            })
        })
    }
}

/// What a text step in pa's editor is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposePurpose {
    Brief,
    Message {
        project: String,
        member: Option<String>,
    },
    Answer {
        project: String,
        member: String,
    },
    EditText {
        project: String,
        field: String,
    },
    DelegateTask {
        project: String,
        worker: Worker,
    },
    Consensus {
        project: String,
    },
    AdoptRef {
        project: String,
    },
    /// One parameter of a factory run being launched from the factory page.
    LaunchParam(Box<Launch>),
    /// A specialist's prompt or description.
    SpecialistText {
        project: String,
        specialist: Box<ProjectSpecialist>,
        field: String,
    },
}

/// A factory launch: the machine, its parameters, and the values entered so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Launch {
    pub machine: String,
    pub params: Vec<(String, String, bool, String)>,
    pub index: usize,
    pub values: Vec<(String, String)>,
}

/// Who runs a new assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Worker {
    /// A crew thread on a native Prime session with this pa model ("" is the default).
    Model {
        model: String,
        thinking: String,
        write: bool,
    },
    /// A factory machine.
    Machine(String),
}

/// Which Captain flow opened pa's shared searchable model + thinking picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CaptainModelTarget {
    Setup {
        current_model: String,
        current_thinking: String,
    },
    CaptainLead {
        project: String,
        current_model: String,
        current_thinking: String,
    },
    Assign {
        project: String,
    },
    Crew {
        project: String,
        member: String,
        label: String,
        current_model: String,
        current_thinking: String,
    },
    FactoryNode {
        project: String,
        name: String,
        state_id: String,
        role: Option<String>,
        current_model: String,
        current_thinking: String,
    },
    /// The factory's `default` role, which every step that uses it runs on.
    FactoryDefault {
        project: String,
        name: String,
        current_model: String,
        current_thinking: String,
    },
    /// Setup: one model for every step of the factory the Captain designs.
    SetupFactory {
        current_model: String,
        current_thinking: String,
    },
}

/// What the session UI does for a key press.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CaptainAction {
    None,
    Close,
    Refresh,
    /// Run `pa thread captain act OP` with `args`; `done` is the notice on success.
    Act {
        op: String,
        args: Value,
        done: String,
    },
    /// Ask the backend what to open, then open it.
    Open {
        project: String,
        member: Option<String>,
    },
    /// Ask for text in pa's prompt editor.
    Compose {
        purpose: ComposePurpose,
        label: String,
        initial: String,
    },
    /// Load the form options for a project ("" for a new one).
    LoadChoices {
        project: Option<String>,
        reference: Option<String>,
    },
    /// Open the git review of a crew thread (an external review tool).
    Review {
        project: String,
        member: String,
    },
    /// Load a project factory for its review page (`factory-show`).
    LoadFactory {
        project: String,
        name: String,
    },
    /// Open pa's machine editor on this project factory; closing it reloads the review page.
    EditFactory {
        root: String,
        name: String,
    },
    /// Open pa's shared searchable model + thinking picker for a Captain flow.
    PickModel(CaptainModelTarget),
    /// Run a chart edit (`factory-compose`, `factory-add-stage`, `factory-remove-stage`), then
    /// open the factory's review page with its interactive diagram.
    SaveFactory {
        op: String,
        project: String,
        name: String,
        args: Value,
    },
    /// Open the live factory page on a Captain-started run.
    OpenFactoryRun {
        project: String,
        member: String,
        run_id: Option<String>,
    },
    /// Load the edit history of a Captain file (`edit-history`).
    LoadHistory {
        project: String,
        field: String,
    },
    /// Open the scoped agents view on a crew chat's subagents.
    OpenSubagents {
        session: String,
    },
}

/// The `member` of a [`CaptainModelTarget::Crew`] pick that sets a specialist's model.
pub(crate) const SPECIALIST_MEMBER: &str = "specialist:";

/// The panel's right-pane tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Digest,
    Drive,
    Factories,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Overview, Tab::Digest, Tab::Drive, Tab::Factories];

    fn label(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Digest => "Digest",
            Tab::Drive => "Drive",
            Tab::Factories => "Factories",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// The Captain's files in the Drive tab, with the project field each one holds.
const DRIVE_FILES: [(&str, &str); 4] = [
    ("AGENTS.md", "rules"),
    ("CREW.md", "crew_rules"),
    ("NOTES.md", "plan"),
    ("DIGEST.md", "digest"),
];

/// A file in the Drive tab: a Captain field or a project specialist.
#[derive(Debug, Clone, PartialEq)]
enum DriveFile {
    Field {
        name: &'static str,
        field: &'static str,
    },
    Specialist(ProjectSpecialist),
    /// A saved crew report or handoff: `dir` is `reports` or `handoffs`.
    Saved {
        dir: &'static str,
        name: String,
    },
}

/// The largest saved report or handoff the Drive previews.
const SAVED_PREVIEW_BYTES: usize = 64 * 1024;

/// The names in `<root>/.prime/captain/<dir>/`, sorted; none when it is missing.
fn saved_names(root: &str, dir: &str) -> Vec<String> {
    if root.is_empty() {
        return Vec::new();
    }
    let path = std::path::Path::new(root).join(".prime/captain").join(dir);
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// A saved file's text for the preview, JSON pretty-printed in a code block.
fn saved_preview(root: &str, dir: &str, name: &str) -> Option<String> {
    if root.is_empty() || name.contains("..") || name.starts_with('/') {
        return None;
    }
    let path = std::path::Path::new(root)
        .join(".prime/captain")
        .join(dir)
        .join(name);
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(SAVED_PREVIEW_BYTES)]).into_owned();
    if name.ends_with(".json") {
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            let pretty = serde_json::to_string_pretty(&value).unwrap_or(text);
            return Some(format!("```json\n{pretty}\n```"));
        }
    }
    Some(text)
}

/// An Overview drawer and its selected row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drawer {
    Wake(usize),
    Schedules(usize),
}

/// One selectable row of the Overview: a human gate or a crew member.
#[derive(Debug, Clone, PartialEq)]
enum Entry {
    Need(NeedsYouItem),
    Member(CaptainMember),
    /// A step subagent of a factory run, indented under it.
    Step {
        member: CaptainMember,
        agent: StepAgent,
    },
}

impl Entry {
    fn key(&self) -> String {
        match self {
            Entry::Need(item) => format!("need:{}", item.id),
            Entry::Member(member) => format!("member:{}", member.id),
            Entry::Step { member, agent } => step_key(&member.id, agent),
        }
    }
}

fn step_key(member: &str, agent: &StepAgent) -> String {
    format!("step:{member}:{}", agent.child_id)
}

/// The `open` member that enters a run's live step subagent.
fn step_open_action(project: &str, agent: &StepAgent) -> CaptainAction {
    CaptainAction::Open {
        project: project.to_owned(),
        member: Some(format!("step:{}", agent.target())),
    }
}

/// The Overview shows this many human gates; the rest are counted.
const NEEDS_SHOWN: usize = 5;

/// The longest specialist prompt the panel holds whole (the backend caps longer ones).
const SPECIALIST_PROMPT_SHOWN: usize = 8000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Projects,
    Crew,
}

/// One row of a picker.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PickItem {
    pub label: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
enum PickPurpose {
    EditField {
        project: String,
    },
    Thinking {
        project: String,
    },
    Workers {
        project: String,
    },
    Delegate {
        project: String,
    },
    DelegateWrite {
        project: String,
        model: String,
        thinking: String,
    },
    Adopt {
        project: String,
    },
    Setup(Box<Setup>),
    Launch,
    /// `request` makes repeated design presses from one list a single request.
    Factories {
        project: String,
        request: String,
    },
    Specialists {
        project: String,
    },
    /// A specialist's field to edit.
    SpecialistField(Box<SpecialistView>),
    /// `m` on a state whose role other states share: the role or only this state.
    RoleOrState {
        review: Box<FactoryReview>,
        state_id: String,
        role: String,
        current_model: String,
        current_thinking: String,
    },
    /// `+` on a chart-built factory: the stage to add.
    AddStage(Box<FactoryReview>),
}

#[derive(Debug, Clone, PartialEq)]
struct Picker {
    title: String,
    items: Vec<PickItem>,
    selected: usize,
    filter: String,
    purpose: PickPurpose,
    /// Rows marked in a multi-select step (the setup's existing threads).
    marked: Vec<usize>,
    multi: bool,
}

impl Picker {
    fn new(title: &str, items: Vec<PickItem>, purpose: PickPurpose) -> Self {
        Self {
            title: title.into(),
            items,
            selected: 0,
            filter: String::new(),
            purpose,
            marked: Vec::new(),
            multi: false,
        }
    }

    fn visible(&self) -> Vec<usize> {
        let filter = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|&i| filter.is_empty() || self.items[i].label.to_lowercase().contains(&filter))
            .collect()
    }

    fn current(&self) -> Option<usize> {
        self.visible().get(self.selected).copied()
    }

    fn select_value(&mut self, value: &Value) {
        if let Some(index) = self
            .visible()
            .iter()
            .position(|&i| self.items[i].value == *value)
        {
            self.selected = index;
        }
    }
}

/// The new-project steps, in order.
pub(crate) const SETUP_STEPS: [&str; 8] = [
    "brief",
    "model",
    "thinking",
    "factory",
    "factory models",
    "workers",
    "threads",
    "confirm",
];

/// The "factory models" step: shown only when the Captain designs a factory.
const FACTORY_MODELS_STEP: usize = 4;

/// The new-project or promote form: one value per step.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Setup {
    pub step: usize,
    pub reference: Option<String>,
    pub brief: String,
    pub provider: String,
    pub model: String,
    pub thinking: String,
    pub machine: Option<String>,
    /// The Captain designs a project factory after it writes its plan.
    pub design: bool,
    /// Whether the factory step has an answer (its picker then selects it).
    pub factory_chosen: bool,
    pub parallel: u32,
    pub crew: Vec<String>,
    pub choices: CaptainChoices,
    /// How the designed factory picks models: "captain", "model", or "choose"; empty when
    /// not asked.
    pub factory_models: String,
    pub factory_model: String,
    pub factory_thinking: String,
    /// The factory the user built in the chart builder: its enabled stages and models.
    pub compose: Option<ComposeChoice>,
}

/// A chart-built factory: `factory-compose`'s ordered stages and `{stage: {model, thinking}}`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ComposeChoice {
    pub stages: Vec<String>,
    pub models: Value,
}

/// The chart builder's stages in their default order: id, what it does, on by default.
/// `captain_factories.py` (`compose_manifest`) wires them; `compose_spec` previews that wiring.
const COMPOSE_STAGES: [(&str, &str, bool); 7] = [
    ("plan", "architectural plan and task breakdown", true),
    (
        "consensus",
        "two models challenge and vote on the plan",
        false,
    ),
    ("implement", "code the change in a worktree", true),
    ("sanitize-diff", "module: clean the implement diff", false),
    ("validate", "module: run the project's checks", true),
    (
        "review-triad",
        "module: 3 reviewers, approve or rework",
        true,
    ),
    ("pr-ready", "final summary and PR prep", false),
];

/// The `member` of a [`CaptainModelTarget::Crew`] pick that sets a chart builder stage's model.
pub(crate) const COMPOSE_MEMBER: &str = "compose-stage:";

/// The stage a state of a chart-built factory belongs to.
fn compose_stage_of(state_id: &str) -> Option<&'static str> {
    COMPOSE_STAGES.iter().map(|(id, _, _)| *id).find(|stage| {
        state_id == *stage
            || (!matches!(*stage, "plan" | "implement" | "pr-ready")
                && state_id
                    .strip_prefix(stage)
                    .is_some_and(|rest| rest.starts_with('-')))
    })
}

/// The one state a stage starts at; fan-out stages start at a `<stage>-start` node.
fn compose_entry(stage: &str) -> String {
    match stage {
        "consensus" | "review-triad" => format!("{stage}-start"),
        "sanitize-diff" => "sanitize-diff-sanitize".into(),
        "validate" => "validate-run".into(),
        _ => stage.into(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ComposeStage {
    pub id: String,
    pub on: bool,
    pub model: String,
    pub thinking: String,
}

/// The visual FSM chart builder: a stage checklist and the live diagram of the enabled stages.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ComposeView {
    /// Empty during setup, before the project exists.
    pub project: String,
    pub name: String,
    pub stages: Vec<ComposeStage>,
    pub selected: usize,
    /// Re-compose an existing chart-built factory.
    pub replace: bool,
    /// Setup returns here after the chart: the choice goes into its `init` request.
    pub setup: Option<Box<Setup>>,
}

impl ComposeView {
    /// `enabled` stages first in their order, then the rest off in the default order.
    fn new(project: String, name: String, enabled: Option<&[String]>, models: &Value) -> Self {
        let model = |id: &str, key: &str| models[id][key].as_str().unwrap_or_default().to_owned();
        let stage = |id: &str, on: bool| ComposeStage {
            id: id.into(),
            on,
            model: model(id, "model"),
            thinking: model(id, "thinking"),
        };
        let stages = match enabled {
            Some(enabled) => enabled
                .iter()
                .filter(|id| COMPOSE_STAGES.iter().any(|(s, _, _)| s == id))
                .map(|id| stage(id, true))
                .chain(
                    COMPOSE_STAGES
                        .iter()
                        .filter(|(id, _, _)| !enabled.iter().any(|e| e == id))
                        .map(|(id, _, _)| stage(id, false)),
                )
                .collect(),
            None => COMPOSE_STAGES
                .iter()
                .map(|(id, _, on)| stage(id, *on))
                .collect(),
        };
        Self {
            project,
            name,
            stages,
            selected: 0,
            replace: false,
            setup: None,
        }
    }

    fn enabled(&self) -> Vec<&ComposeStage> {
        self.stages.iter().filter(|s| s.on).collect()
    }

    fn choice(&self) -> ComposeChoice {
        let models: serde_json::Map<String, Value> = self
            .enabled()
            .into_iter()
            .filter(|s| !s.model.is_empty())
            .map(|s| {
                (
                    s.id.clone(),
                    json!({"model": s.model, "thinking": s.thinking}),
                )
            })
            .collect();
        ComposeChoice {
            stages: self.enabled().iter().map(|s| s.id.clone()).collect(),
            models: Value::Object(models),
        }
    }

    /// Why the backend would refuse these stages, as `checked_stages` does.
    fn problem(&self) -> Option<&'static str> {
        let stages = self.choice().stages;
        let at = |id: &str| stages.iter().position(|s| s == id);
        match stages.first().map(String::as_str) {
            None => Some("Turn on at least one stage."),
            Some("plan" | "implement") => match (at("implement"), at("sanitize-diff")) {
                (implement, Some(sanitize)) if implement.is_none_or(|i| i > sanitize) => {
                    Some("Put implement before sanitize-diff: it cleans the implement diff.")
                }
                _ => None,
            },
            Some(_) => Some("Start the factory with plan or implement."),
        }
    }

    /// The compiled-spec shape of the enabled stages, wired as `compose_manifest` wires them,
    /// with the rework loops back to implement (or plan).
    fn spec(&self) -> Value {
        let enabled = self.enabled();
        let mut states: Vec<Value> = Vec::new();
        let mut transitions: Vec<Value> = Vec::new();
        let agent = |id: &str, entry: bool, stage: &ComposeStage| {
            let mut subagent = json!({});
            if !stage.model.is_empty() {
                subagent["model"] = json!(stage.model.rsplit('/').next().unwrap_or(&stage.model));
                if !stage.thinking.is_empty() {
                    subagent["thinking"] = json!(stage.thinking);
                }
            }
            json!({"id": id, "entry": entry, "max_entries": 4, "subagent": subagent})
        };
        let python = |id: &str, entry: bool, callable: &str| json!({"id": id, "entry": entry, "python": {"import": "nodes", "callable": callable}});
        let edge = |from: Value, to: &str| json!({"from": from, "to": to});
        let when = |from: &str, to: &str, output: &str, value: Value| json!({"from": from, "to": to, "when": {"output": output, "op": "eq", "value": value}});
        for (i, stage) in enabled.iter().enumerate() {
            let id = stage.id.as_str();
            let entry = i == 0;
            let earlier = |s: &str| enabled[..i].iter().any(|e| e.id == s);
            let after = enabled
                .get(i + 1)
                .map_or_else(|| "done".to_owned(), |next| compose_entry(&next.id));
            let rework = if earlier("implement") {
                "implement".to_owned()
            } else if earlier("plan") {
                "plan".to_owned()
            } else {
                after.clone()
            };
            match id {
                "consensus" | "review-triad" => {
                    let start = compose_entry(id);
                    let (sides, join): (Vec<String>, String) = if id == "consensus" {
                        (
                            vec!["consensus-a".into(), "consensus-b".into()],
                            "consensus-vote".into(),
                        )
                    } else {
                        (
                            ["correctness", "bugs", "robustness"]
                                .iter()
                                .map(|r| format!("review-triad-{r}"))
                                .collect(),
                            "review-triad-verdict".into(),
                        )
                    };
                    states.push(python(&start, entry, "start"));
                    for side in &sides {
                        states.push(agent(side, false, stage));
                        transitions.push(edge(json!(start), side));
                    }
                    states.push(python(&join, false, "vote"));
                    transitions.push(edge(json!(sides), &join));
                    let (yes, no, back) = if id == "consensus" {
                        let back = if earlier("plan") { "plan" } else { &after };
                        ("approve", "revise", back.to_owned())
                    } else {
                        ("approve", "rework", rework.clone())
                    };
                    transitions.push(when(&join, &after, "verdict", json!(yes)));
                    transitions.push(when(&join, &back, "verdict", json!(no)));
                }
                "sanitize-diff" => {
                    states.push(python("sanitize-diff-sanitize", entry, "sanitize"));
                    transitions.push(edge(json!("sanitize-diff-sanitize"), &after));
                }
                "validate" => {
                    states.push(python("validate-run", entry, "run"));
                    transitions.push(when("validate-run", &after, "passed", json!(true)));
                    transitions.push(when("validate-run", &rework, "passed", json!(false)));
                }
                _ => {
                    states.push(agent(id, entry, stage));
                    transitions.push(edge(json!(id), &after));
                }
            }
        }
        if !enabled.is_empty() {
            states.push(python("done", false, "done"));
        }
        json!({"states": states, "transitions": transitions})
    }

    /// The selected row's stage moves one place up (`up`) or down.
    fn reorder(&mut self, up: bool) {
        let to = if up {
            self.selected.checked_sub(1)
        } else {
            Some(self.selected + 1).filter(|&i| i < self.stages.len())
        };
        if let Some(to) = to {
            self.stages.swap(self.selected, to);
            self.selected = to;
        }
    }
}

impl Setup {
    fn request(&self) -> Value {
        let mut args = json!({
            "brief": self.brief,
            "provider": if self.provider.is_empty() { "prime" } else { &self.provider },
            "model": self.model,
            "thinking": self.thinking,
            "parallel": self.parallel,
            "crew": self.crew,
        });
        if let Some(reference) = &self.reference {
            args["ref"] = json!(reference);
        }
        if let Some(machine) = &self.machine {
            args["machine"] = json!(machine);
        }
        if self.design {
            args["design"] = json!(true);
            if !self.factory_models.is_empty() {
                let mut choice = json!({"mode": self.factory_models});
                if self.factory_models == "model" {
                    choice["model"] = json!(self.factory_model);
                    choice["thinking"] = json!(self.factory_thinking);
                }
                args["factory_models"] = choice;
            }
        }
        if let Some(compose) = &self.compose {
            args["compose"] = json!({
                "name": "pipeline",
                "stages": compose.stages,
                "models": compose.models,
            });
        }
        args
    }

    /// The step after (`forward`) or before the current one, skipping "factory models" unless
    /// the Captain designs a factory.
    fn next_step(&self, forward: bool) -> usize {
        let step = if forward {
            self.step + 1
        } else {
            self.step.saturating_sub(1)
        };
        if step == FACTORY_MODELS_STEP && !self.design {
            if forward {
                step + 1
            } else {
                step - 1
            }
        } else {
            step
        }
    }

    fn factory_models_summary(&self) -> String {
        match self.factory_models.as_str() {
            "model" if self.factory_thinking.is_empty() => self.factory_model.clone(),
            "model" => format!("{} · {}", self.factory_model, self.factory_thinking),
            "choose" => "Captain chooses per step".into(),
            _ => "same as the Captain".into(),
        }
    }

    /// The factory step's picker value for the current answer.
    fn factory_value(&self) -> Value {
        match &self.machine {
            _ if self.design => json!({"design": true}),
            _ if self.compose.is_some() => json!({"compose": true}),
            Some(machine) => json!(machine),
            None => Value::Null,
        }
    }

    fn factory_summary(&self) -> String {
        match (&self.machine, &self.compose) {
            _ if self.design => "Captain designs one project FSM".into(),
            (_, Some(compose)) => format!("custom chart · {}", compose.stages.join(" → ")),
            (Some(machine), None) => machine.clone(),
            (None, None) => "none for now (Captain + crew only)".into(),
        }
    }
}

/// The review page's default model: the factory's `default` role, or the model that steps with
/// no model or role inherit.
fn factory_default_label(review: &FactoryReview) -> String {
    match &review.default_role {
        Some((model, thinking)) if thinking.is_empty() => format!("{model} (role default)"),
        Some((model, thinking)) => format!("{model} · {thinking} (role default)"),
        None => crate::factory_view::inherit_label(&review.inherit_model),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Confirm {
    Stop {
        project: String,
        name: String,
    },
    Delete {
        project: String,
        name: String,
        crew: usize,
        running: usize,
    },
    Release {
        project: String,
        member: String,
        title: String,
    },
    Create(Box<Setup>),
    DeleteFactory(Box<FactoryReview>),
    /// Delete a factory from the Factories tab; cancel returns to the tab.
    DeleteListedFactory {
        project: String,
        name: String,
        path: String,
    },
    DeleteSpecialist(Box<SpecialistView>),
    /// `x` on a factory diagram: remove the selected node's stage.
    RemoveStage {
        review: Box<FactoryReview>,
        stage: String,
    },
}

/// The review page of one project factory: its state, errors, and interactive diagram.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct FactoryReview {
    pub project: String,
    pub name: String,
    pub description: String,
    pub params: Vec<Value>,
    pub state_details: Value,
    pub state: String,
    pub approved: bool,
    pub path: String,
    pub file: String,
    pub digest: String,
    /// The digest from the user's last explicit load (`F` or `r`): the version they saw.
    /// A background refresh with another digest blocks approval until `r`.
    pub reviewed: String,
    pub errors: Vec<String>,
    /// The compiled spec (`pa factory compile`), when the factory is valid.
    pub graph: Option<Value>,
    pub graph_error: Option<String>,
    /// A Captain run of this factory is queued or running.
    pub live: bool,
    /// Vendored modules from `modules.lock`: alias, module, version, latest, state.
    pub modules: Vec<Value>,
    /// Vendored specialists from `modules.lock`: id, states, state.
    pub specialists: Vec<Value>,
    pub scroll: usize,
    /// True after manual PageUp/PageDown until the next node selection step.
    pub manual_scroll: bool,
    /// Currently highlighted node in the interactive block diagram.
    pub selected_node: Option<String>,
    /// Open node detail/files view: `(state_id, scroll_offset)`.
    pub node_detail: Option<(String, usize)>,
    /// The model steps with no model or role run on (the project's factory default);
    /// empty means pa's default model.
    pub inherit_model: String,
    /// The factory's `default` role: (model, thinking).
    pub default_role: Option<(String, String)>,
}

/// A provider CLI thread's transcript and reply state.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ThreadView {
    pub project: String,
    pub member: String,
    pub title: String,
    pub provider: String,
    pub messages: Vec<(String, String)>,
    pub busy: bool,
    pub scroll: usize,
    /// Opened from F7 or F8 over a chat: Esc closes the panel back to that chat.
    pub returns_to_chat: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Mode {
    Browse,
    Pick(Picker),
    Confirm(Confirm),
    Thread(ThreadView),
    Factory(FactoryReview),
    Compose(ComposeView),
    Specialist(SpecialistView),
    History(HistoryView),
}

/// One project specialist: its prompt, with model, edit, and delete keys.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SpecialistView {
    pub project: String,
    pub specialist: ProjectSpecialist,
    pub scroll: usize,
}

/// The edit history of one Captain file, from `edit-history`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct HistoryView {
    pub project: String,
    pub field: String,
    pub edits: Vec<Value>,
    pub selected: usize,
}

pub(crate) struct CaptainView {
    snapshot: CaptainSnapshot,
    loaded: bool,
    /// The last action's error; it stays until the next key press.
    error: Option<String>,
    /// The last status read's error; the next good read clears it.
    snapshot_error: Option<String>,
    notice: Option<String>,
    project: usize,
    /// The selected Overview entry (needs-you items, then the crew).
    member: usize,
    focus: Focus,
    tab: Tab,
    /// The Digest tab's scroll.
    digest_scroll: usize,
    /// The Drive tab's selected file and its preview scroll.
    drive: usize,
    drive_scroll: usize,
    /// The Factories tab's selected factory.
    factory: usize,
    /// One request id per Factories tab visit, so repeated design presses dedupe.
    design_request: String,
    show_archived: bool,
    help: bool,
    mode: Mode,
    viewport_rows: usize,
    /// Setup steps wait for the backend's options.
    pending_setup: Option<Setup>,
    /// A factory launch waits for the machine list.
    pending_launch: bool,
    /// The machines' parameters, for the launch form.
    machine_params: std::collections::BTreeMap<String, Vec<MachineParam>>,
    /// Subagents per crew session path, from the daemon roster.
    subagents: std::collections::HashMap<String, usize>,
    /// The digest each factory had when its review page last loaded explicitly, by
    /// `project/name`: `a` approves from a list only at that digest.
    reviewed: std::collections::HashMap<String, String>,
    /// A panel mounted only for one provider thread or a launch; its close is not parked.
    transient: bool,
    /// The Overview's Notifications Inbox while open (`i`), with its selected row.
    inbox: Option<usize>,
    /// The Overview's Wake Policies (`w`) or Schedules (`S`) drawer while open.
    drawer: Option<Drawer>,
}

/// Collapse whitespace to one line.
fn clean(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The glyph of an activity state or an F7 chat status.
pub(crate) fn activity_glyph(state: &str) -> (&'static str, ThemeColor) {
    match state {
        "needs_you" => ("!", ThemeColor::Warning),
        "working" => ("●", ThemeColor::Accent),
        "waiting" | "paused" => ("◐", ThemeColor::Warning),
        "failed" => ("✗", ThemeColor::Error),
        _ => ("·", ThemeColor::Dim),
    }
}

/// A short relative age: `now`, `5m`, `2h`, `3d`.
pub(crate) fn age_label(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    match seconds {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn status_glyph(state: &str) -> (&'static str, ThemeColor) {
    match state {
        "needs_you" => ("!", ThemeColor::Warning),
        "running" => ("●", ThemeColor::Accent),
        "ready" => ("✓", ThemeColor::Success),
        "done" => ("·", ThemeColor::Dim),
        "failed" => ("×", ThemeColor::Error),
        _ => ("?", ThemeColor::Dim),
    }
}

fn summary_color(state: &str) -> ThemeColor {
    match state {
        "needs you" => ThemeColor::Warning,
        "running" => ThemeColor::Accent,
        "paused" | "stopping" => ThemeColor::Error,
        _ => ThemeColor::Dim,
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut rows = Vec::new();
    for paragraph in text.lines() {
        let mut row = String::new();
        for word in paragraph.split_whitespace() {
            if !row.is_empty() && str_width(&row) + 1 + str_width(word) > width {
                rows.push(std::mem::take(&mut row));
            }
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(word);
        }
        rows.push(row);
    }
    rows
}

impl CaptainView {
    pub(crate) fn new(viewport_rows: usize) -> Self {
        Self {
            snapshot: CaptainSnapshot::default(),
            loaded: false,
            error: None,
            snapshot_error: None,
            notice: None,
            project: 0,
            member: 0,
            focus: Focus::Projects,
            tab: Tab::Overview,
            digest_scroll: 0,
            drive: 0,
            drive_scroll: 0,
            factory: 0,
            design_request: String::new(),
            show_archived: false,
            drawer: None,
            help: false,
            mode: Mode::Browse,
            viewport_rows: viewport_rows.max(12),
            pending_setup: None,
            pending_launch: false,
            machine_params: std::collections::BTreeMap::new(),
            subagents: std::collections::HashMap::new(),
            reviewed: std::collections::HashMap::new(),
            transient: false,
            inbox: None,
        }
    }

    pub(crate) fn set_viewport_rows(&mut self, rows: usize) {
        self.viewport_rows = rows.max(10);
    }

    /// Mark the panel as mounted for one flow only: its close is not parked.
    pub(crate) fn set_transient(&mut self) {
        self.transient = true;
    }

    pub(crate) fn is_transient(&self) -> bool {
        self.transient
    }

    /// Ready the panel to park across a close: the browse page keeps its project, focus,
    /// selection, tab, and scroll; a sub-page, help, and messages go.
    pub(crate) fn park(&mut self) {
        self.mode = Mode::Browse;
        self.help = false;
        self.inbox = None;
        self.notice = None;
        self.error = None;
    }

    /// The subagent count of each crew session, from the daemon roster.
    pub(crate) fn set_subagents(&mut self, counts: std::collections::HashMap<String, usize>) {
        self.subagents = counts;
    }

    fn subagent_count(&self, member: &CaptainMember) -> usize {
        member
            .session
            .as_ref()
            .and_then(|s| self.subagents.get(s))
            .copied()
            .unwrap_or(0)
    }

    /// Apply a fresh status reply; the selection follows the same project and entry.
    pub(crate) fn apply_snapshot(&mut self, data: &Value) {
        let selected_project = self.selected_project().map(|p| p.id.clone());
        let selected_entry = self.selected_entry().map(|e| e.key());
        match serde_json::from_value::<CaptainSnapshot>(data.clone()) {
            Ok(snapshot) => {
                let first = !self.loaded;
                self.snapshot = snapshot;
                self.loaded = true;
                self.snapshot_error = None;
                let target = if first {
                    let here = &self.snapshot.current_root;
                    self.snapshot
                        .projects
                        .iter()
                        .position(|p| &p.root == here)
                        .or_else(|| {
                            let current = self.snapshot.current.as_ref()?;
                            self.snapshot.projects.iter().position(|p| &p.id == current)
                        })
                } else {
                    selected_project
                        .and_then(|id| self.snapshot.projects.iter().position(|p| p.id == id))
                };
                self.project = target
                    .unwrap_or(self.project)
                    .min(self.snapshot.projects.len().saturating_sub(1));
                if let Some(key) = selected_entry {
                    if let Some(index) = self.entries().iter().position(|e| e.key() == key) {
                        self.member = index;
                    }
                }
                self.clamp_member();
            }
            Err(error) => {
                self.snapshot_error = Some(format!("Captain returned invalid status: {error}"));
            }
        }
    }

    pub(crate) fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }

    pub(crate) fn set_snapshot_error(&mut self, error: String) {
        self.snapshot_error = Some(error);
    }

    pub(crate) fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    /// Focus a project by name, id, or root (the `/captain <name>` argument).
    pub(crate) fn focus_project(&mut self, query: &str) -> bool {
        let term = query.trim().to_lowercase();
        let found = self.snapshot.projects.iter().position(|p| {
            p.id == term || p.name.to_lowercase() == term || p.root.to_lowercase() == term
        });
        let found = found.or_else(|| {
            self.snapshot
                .projects
                .iter()
                .position(|p| p.name.to_lowercase().contains(&term))
        });
        if let Some(index) = found {
            self.project = index;
            self.focus = Focus::Projects;
            self.clamp_member();
        }
        found.is_some()
    }

    pub(crate) fn snapshot(&self) -> &CaptainSnapshot {
        &self.snapshot
    }

    pub(crate) fn selected_project(&self) -> Option<&CaptainProject> {
        self.snapshot.projects.get(self.project)
    }

    /// The Overview's selectable rows: the first needs-you items, then the crew.
    fn entries(&self) -> Vec<Entry> {
        let Some(project) = self.selected_project() else {
            return Vec::new();
        };
        let mut entries: Vec<Entry> = project
            .needs_items()
            .into_iter()
            .take(NEEDS_SHOWN)
            .map(Entry::Need)
            .collect();
        for member in project.flat_crew(self.show_archived) {
            entries.push(Entry::Member(member.clone()));
            entries.extend(member.shown_steps().into_iter().map(|agent| Entry::Step {
                member: member.clone(),
                agent: agent.clone(),
            }));
        }
        entries
    }

    fn selected_entry(&self) -> Option<Entry> {
        if self.focus != Focus::Crew {
            return None;
        }
        self.entries().into_iter().nth(self.member)
    }

    fn selected_need(&self) -> Option<NeedsYouItem> {
        match self.selected_entry()? {
            Entry::Need(item) => Some(item),
            Entry::Member(_) | Entry::Step { .. } => None,
        }
    }

    fn selected_step(&self) -> Option<StepAgent> {
        match self.selected_entry()? {
            Entry::Step { agent, .. } => Some(agent),
            _ => None,
        }
    }

    /// Clear a member's unread mark as the user opens it; the backend's open records it seen.
    fn mark_seen(&mut self, member: &str) {
        let Some(project) = self.snapshot.projects.get_mut(self.project) else {
            return;
        };
        if let Some(m) = project.crew.iter_mut().find(|m| m.id == member) {
            project.unread_count = (project.unread_count - m.unread_count).max(0);
            m.unread = false;
            m.unread_count = 0;
        }
        project.unread = project.crew.iter().any(|m| m.unread);
    }

    /// The selected crew member: its row, or the member a selected needs-you item names.
    fn selected_member(&self) -> Option<CaptainMember> {
        match self.selected_entry()? {
            Entry::Member(member) | Entry::Step { member, .. } => Some(member),
            Entry::Need(item) => {
                let id = item.member?;
                self.selected_project()?
                    .crew
                    .iter()
                    .find(|m| m.id == id)
                    .cloned()
            }
        }
    }

    fn clamp_member(&mut self) {
        let count = self.entries().len();
        self.member = self.member.min(count.saturating_sub(1));
        if count == 0 && self.focus == Focus::Crew {
            self.focus = Focus::Projects;
        }
    }

    /// The project at the current checkout, if it has a Captain.
    fn here_has_captain(&self) -> bool {
        self.snapshot
            .projects
            .iter()
            .any(|p| p.root == self.snapshot.current_root)
    }

    /// Start the new-project form; `reference` promotes that conversation instead.
    pub(crate) fn start_setup(&mut self, reference: Option<String>) -> CaptainAction {
        if self.here_has_captain() {
            self.notice = Some("This project already has a Captain.".into());
            return CaptainAction::None;
        }
        self.notice = Some("Loading models and machines…".into());
        self.pending_setup = Some(Setup {
            reference: reference.clone(),
            parallel: 2,
            provider: "prime".into(),
            ..Setup::default()
        });
        CaptainAction::LoadChoices {
            project: None,
            reference,
        }
    }

    /// The backend's form options arrived.
    pub(crate) fn apply_choices(&mut self, data: &Value, project: Option<&str>) -> CaptainAction {
        let choices: CaptainChoices = match serde_json::from_value(data.clone()) {
            Ok(choices) => choices,
            Err(error) => {
                self.error = Some(format!("Captain returned invalid options: {error}"));
                return CaptainAction::None;
            }
        };
        if std::mem::take(&mut self.pending_launch) {
            self.machine_params = choices.machine_params.clone();
            let items = choices
                .machines
                .iter()
                .map(|name| PickItem {
                    label: name.clone(),
                    value: json!(name),
                })
                .collect::<Vec<_>>();
            if items.is_empty() {
                self.notice = Some("No machines installed.".into());
                return CaptainAction::Close;
            }
            self.mode = Mode::Pick(Picker::new(
                "launch a factory run",
                items,
                PickPurpose::Launch,
            ));
            return CaptainAction::None;
        }
        if let Some(mut setup) = self.pending_setup.take() {
            self.notice = None;
            setup.brief = choices.brief.clone().unwrap_or_default();
            setup.choices = choices;
            return self.setup_step(setup);
        }
        let Some(project) = project.map(str::to_owned) else {
            return CaptainAction::None;
        };
        if let Mode::Pick(picker) = &self.mode {
            match picker.purpose {
                PickPurpose::Delegate { .. } => self.open_delegate_picker(project, &choices),
                PickPurpose::Adopt { .. } => self.open_adopt_picker(project, &choices),
                _ => {}
            }
        }
        CaptainAction::None
    }

    fn open_delegate_picker(&mut self, project: String, choices: &CaptainChoices) {
        let mut items: Vec<PickItem> = vec![PickItem {
            label: "thread · pick model…".into(),
            value: json!({"pick_model": true}),
        }];
        items.extend(choices.crew_models.iter().map(|m| PickItem {
            label: format!("thread · {}", m.label),
            value: json!({"model": m.model}),
        }));
        items.extend(choices.machines.iter().map(|name| PickItem {
            label: format!("factory · {name}"),
            value: json!({"machine": name}),
        }));
        items.extend(choices.project_factories.iter().map(|name| PickItem {
            label: format!("project factory · {name}"),
            value: json!({"machine": name}),
        }));
        self.mode = Mode::Pick(Picker::new(
            "assign work",
            items,
            PickPurpose::Delegate { project },
        ));
    }

    /// The shared model picker returned a model and thinking level for Captain setup.
    pub(crate) fn apply_setup_model(&mut self, model: String, thinking: String) -> CaptainAction {
        let Some(mut setup) = self.pending_setup.take() else {
            return CaptainAction::None;
        };
        setup.provider = "prime".into();
        setup.model = model;
        setup.thinking = thinking;
        // Skip the separate thinking step: the shared picker sets both model and thinking.
        setup.step = 3;
        self.setup_step(setup)
    }

    /// The shared model picker returned the model for every step of the designed factory.
    pub(crate) fn apply_setup_factory_model(
        &mut self,
        model: String,
        thinking: String,
    ) -> CaptainAction {
        let Some(mut setup) = self.pending_setup.take() else {
            return CaptainAction::None;
        };
        setup.factory_models = "model".into();
        setup.factory_model = model;
        setup.factory_thinking = thinking;
        setup.step = FACTORY_MODELS_STEP + 1;
        self.setup_step(setup)
    }

    /// Esc in the shared model picker returns to the "factory models" step.
    pub(crate) fn cancel_setup_factory_model(&mut self) -> CaptainAction {
        let Some(mut setup) = self.pending_setup.take() else {
            self.mode = Mode::Browse;
            return CaptainAction::None;
        };
        setup.step = FACTORY_MODELS_STEP;
        self.setup_step(setup)
    }

    /// Esc in the shared model picker during Captain setup steps back to the brief.
    pub(crate) fn cancel_setup_model(&mut self) -> CaptainAction {
        let Some(mut setup) = self.pending_setup.take() else {
            self.mode = Mode::Browse;
            return CaptainAction::None;
        };
        setup.step = 0;
        self.setup_step(setup)
    }

    /// The shared model picker returned a model and thinking level for crew assignment.
    pub(crate) fn apply_assign_model(&mut self, project: String, model: String, thinking: String) {
        let items = vec![
            PickItem {
                label: "read · works in the shared checkout".into(),
                value: json!(false),
            },
            PickItem {
                label: "write · works in its own worktree".into(),
                value: json!(true),
            },
        ];
        self.mode = Mode::Pick(Picker::new(
            "access",
            items,
            PickPurpose::DelegateWrite {
                project,
                model,
                thinking,
            },
        ));
    }

    /// `a` here and in the crew switcher (F7): pick an existing thread to add to the crew.
    pub(crate) fn start_adopt(&mut self, project: String) -> CaptainAction {
        self.mode = Mode::Pick(Picker::new(
            "add a thread",
            Vec::new(),
            PickPurpose::Adopt {
                project: project.clone(),
            },
        ));
        CaptainAction::LoadChoices {
            project: Some(project),
            reference: None,
        }
    }

    fn open_adopt_picker(&mut self, project: String, choices: &CaptainChoices) {
        let mut items: Vec<PickItem> = choices
            .adopt
            .iter()
            .map(|a| PickItem {
                label: clean(&a.label),
                value: json!(a.reference),
            })
            .collect();
        items.push(PickItem {
            label: "enter a thread ID or session path".into(),
            value: Value::Null,
        });
        self.mode = Mode::Pick(Picker::new(
            "add a thread",
            items,
            PickPurpose::Adopt { project },
        ));
    }

    /// Show the setup step `setup.step`; text steps go to pa's editor.
    fn setup_step(&mut self, setup: Setup) -> CaptainAction {
        let step = SETUP_STEPS.get(setup.step).copied().unwrap_or("confirm");
        let boxed = |s: &Setup| PickPurpose::Setup(Box::new(s.clone()));
        let title = |name: &str| {
            format!(
                "{} · {name}",
                if setup.reference.is_some() {
                    "promote"
                } else {
                    "new captain"
                }
            )
        };
        match step {
            "brief" => {
                let initial = setup.brief.clone();
                self.pending_setup = Some(setup);
                CaptainAction::Compose {
                    purpose: ComposePurpose::Brief,
                    label: "captain brief · what should this project achieve".into(),
                    initial,
                }
            }
            "model" => {
                let current_model = setup.model.clone();
                let current_thinking = setup.thinking.clone();
                self.pending_setup = Some(setup);
                CaptainAction::PickModel(CaptainModelTarget::Setup {
                    current_model,
                    current_thinking,
                })
            }
            "thinking" => {
                let items = setup
                    .choices
                    .thinking
                    .iter()
                    .map(|level| PickItem {
                        label: level.clone(),
                        value: json!(if level == "default" {
                            ""
                        } else {
                            level.as_str()
                        }),
                    })
                    .collect();
                let mut picker = Picker::new(&title("thinking"), items, boxed(&setup));
                picker.select_value(&json!(setup.thinking));
                self.mode = Mode::Pick(picker);
                CaptainAction::None
            }
            "factory" => {
                let items = vec![
                    PickItem {
                        label: "Design one project FSM (recommended) · the Captain builds one end-to-end pipeline from the brief".into(),
                        value: json!({"design": true}),
                    },
                    PickItem {
                        label: "Build custom FSM from chart · pick, order, and model its stages".into(),
                        value: json!({"compose": true}),
                    },
                    PickItem {
                        label: "None for now (Captain + crew only)".into(),
                        value: Value::Null,
                    },
                ];
                let mut picker = Picker::new(&title("factory"), items, boxed(&setup));
                if setup.factory_chosen {
                    picker.select_value(&setup.factory_value());
                }
                self.mode = Mode::Pick(picker);
                CaptainAction::None
            }
            "factory models" => {
                let items = vec![
                    PickItem {
                        label: "Same as the Captain".into(),
                        value: json!("captain"),
                    },
                    PickItem {
                        label: "Pick one model for every step".into(),
                        value: json!("model"),
                    },
                    PickItem {
                        label: "Captain chooses per step".into(),
                        value: json!("choose"),
                    },
                ];
                let mut picker = Picker::new(&title("factory models"), items, boxed(&setup));
                if !setup.factory_models.is_empty() {
                    picker.select_value(&json!(setup.factory_models));
                }
                self.mode = Mode::Pick(picker);
                CaptainAction::None
            }
            "workers" => {
                let items = [1u32, 2, 3, 4, 6, 8, 12, 16]
                    .iter()
                    .map(|n| PickItem {
                        label: format!("{n} parallel crew turns"),
                        value: json!(n),
                    })
                    .collect();
                let mut picker = Picker::new(&title("worker limit"), items, boxed(&setup));
                picker.select_value(&json!(setup.parallel));
                self.mode = Mode::Pick(picker);
                CaptainAction::None
            }
            "threads" => {
                let items = setup
                    .choices
                    .adopt
                    .iter()
                    .map(|a| PickItem {
                        label: clean(&a.label),
                        value: json!(a.reference),
                    })
                    .collect::<Vec<_>>();
                let mut picker = Picker::new(&title("add existing threads"), items, boxed(&setup));
                picker.multi = true;
                picker.marked = picker
                    .items
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| {
                        item.value
                            .as_str()
                            .is_some_and(|r| setup.crew.iter().any(|c| c == r))
                    })
                    .map(|(i, _)| i)
                    .collect();
                self.mode = Mode::Pick(picker);
                CaptainAction::None
            }
            _ => {
                self.mode = Mode::Confirm(Confirm::Create(Box::new(setup)));
                CaptainAction::None
            }
        }
    }

    /// The factory page's new run: pick a machine, then enter its parameters in pa's editor.
    pub(crate) fn start_launch(&mut self) -> CaptainAction {
        self.pending_launch = true;
        self.mode = Mode::Pick(Picker::new(
            "launch a factory run",
            Vec::new(),
            PickPurpose::Launch,
        ));
        CaptainAction::LoadChoices {
            project: None,
            reference: None,
        }
    }

    /// The next launch step: a parameter in pa's editor, or the finished draft.
    fn launch_step(launch: Launch) -> CaptainAction {
        use std::fmt::Write as _;
        let Some((name, kind, required, description)) = launch.params.get(launch.index) else {
            let params: serde_json::Map<String, Value> = launch
                .values
                .into_iter()
                .map(|(name, value)| (name, Value::String(value)))
                .collect();
            return CaptainAction::Act {
                op: "launch".into(),
                args: json!({"machine": launch.machine, "params": params}),
                done: String::new(),
            };
        };
        let mut label = format!("{} · {name}", launch.machine);
        if kind != "text" {
            let _ = write!(label, " ({kind}, JSON)");
        }
        if !description.is_empty() {
            let _ = write!(label, " · {}", clean(description));
        }
        if !required {
            label.push_str(" · empty keeps the default");
        }
        CaptainAction::Compose {
            purpose: ComposePurpose::LaunchParam(Box::new(launch)),
            label,
            initial: String::new(),
        }
    }

    /// Text from pa's editor for a compose step; `None` when the user cancelled.
    pub(crate) fn compose_done(
        &mut self,
        purpose: ComposePurpose,
        text: Option<String>,
    ) -> CaptainAction {
        if let ComposePurpose::LaunchParam(launch) = &purpose {
            let mut launch = (**launch).clone();
            let Some(text) = text.map(|t| t.trim().to_owned()) else {
                self.notice = Some("Launch cancelled.".into());
                return CaptainAction::Close;
            };
            let required = launch.params[launch.index].2;
            if text.is_empty() && required {
                self.notice = Some(format!(
                    "{} requires a value.",
                    launch.params[launch.index].0
                ));
                return Self::launch_step(launch);
            }
            let name = launch.params[launch.index].0.clone();
            launch.values.push((name, text));
            launch.index += 1;
            return Self::launch_step(launch);
        }
        let Some(text) = text.map(|t| t.trim().to_owned()) else {
            if purpose == ComposePurpose::Brief {
                if let Some(mut setup) = self.pending_setup.take() {
                    if setup.step > 0 {
                        setup.step -= 1;
                        return self.setup_step(setup);
                    }
                }
                self.mode = Mode::Browse;
                self.notice = Some("Setup cancelled.".into());
            }
            return CaptainAction::None;
        };
        let empty_ok = match &purpose {
            ComposePurpose::EditText { .. } => true,
            ComposePurpose::SpecialistText { field, .. } => field != "prompt",
            _ => false,
        };
        if text.is_empty() && !empty_ok {
            self.notice = Some("Nothing entered.".into());
            if purpose == ComposePurpose::Brief {
                if let Some(setup) = self.pending_setup.take() {
                    return self.setup_step(setup);
                }
            }
            return CaptainAction::None;
        }
        match purpose {
            ComposePurpose::Brief => {
                let Some(mut setup) = self.pending_setup.take() else {
                    return CaptainAction::None;
                };
                setup.brief = text;
                setup.step = 1;
                self.setup_step(setup)
            }
            ComposePurpose::Message { project, member } => {
                let (op, args) = match &member {
                    Some(member) => (
                        "reply",
                        json!({"project": project, "member": member, "text": text}),
                    ),
                    None => ("lead", json!({"project": project, "text": text})),
                };
                CaptainAction::Act {
                    op: op.into(),
                    args,
                    done: "Message sent.".into(),
                }
            }
            ComposePurpose::Answer { project, member } => CaptainAction::Act {
                op: "answer".into(),
                args: json!({"project": project, "member": member, "text": text, "decision": "resume"}),
                done: "Answer sent. The run resumes.".into(),
            },
            ComposePurpose::EditText { project, field } => CaptainAction::Act {
                op: "edit".into(),
                args: json!({"project": project, "field": field, "value": text}),
                done: format!("Updated {}.", field.replace('_', " ")),
            },
            ComposePurpose::DelegateTask { project, worker } => {
                let args = match worker {
                    Worker::Model {
                        model,
                        thinking,
                        write,
                    } => {
                        let mut map = json!({
                            "project": project, "task": text, "provider": "prime",
                            "model": model, "write": write,
                        });
                        if !thinking.is_empty() {
                            map["thinking"] = json!(thinking);
                        }
                        map
                    }
                    Worker::Machine(machine) => {
                        json!({"project": project, "task": text, "machine": machine})
                    }
                };
                CaptainAction::Act {
                    op: "delegate".into(),
                    args,
                    done: "Assignment started.".into(),
                }
            }
            ComposePurpose::Consensus { project } => CaptainAction::Act {
                op: "consensus".into(),
                args: json!({"project": project, "task": text}),
                done: String::new(),
            },
            ComposePurpose::LaunchParam(_) => CaptainAction::None,
            ComposePurpose::SpecialistText {
                project,
                specialist,
                field,
            } => {
                let mut specialist = *specialist;
                if field == "prompt" {
                    specialist.prompt = text;
                } else {
                    specialist.description = text;
                }
                Self::specialist_write(&project, &specialist)
            }
            ComposePurpose::AdoptRef { project } => CaptainAction::Act {
                op: "adopt".into(),
                args: json!({"project": project, "ref": text}),
                done: "Thread added to crew.".into(),
            },
        }
    }

    /// Open the read-and-reply view for a provider CLI thread.
    pub(crate) fn open_thread(&mut self, project: String, member: String, data: &Value) {
        let messages = data
            .get("messages")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| {
                        Some((
                            row.get(0)?.as_str()?.to_owned(),
                            row.get(1)?.as_str()?.to_owned(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let found = self
            .snapshot
            .projects
            .iter()
            .find(|p| p.id == project)
            .and_then(|p| p.crew.iter().find(|m| m.id == member))
            .map(CaptainMember::label);
        // A refresh of the open thread keeps its scroll; a new thread starts at the end.
        let (scroll, returns_to_chat, title) = match &self.mode {
            Mode::Thread(open) if open.member == member => (
                open.scroll,
                open.returns_to_chat,
                found.unwrap_or_else(|| open.title.clone()),
            ),
            _ => {
                self.notice = None;
                (0, false, found.unwrap_or_else(|| "captain".into()))
            }
        };
        self.mode = Mode::Thread(ThreadView {
            project,
            member,
            title,
            provider: data["provider"].as_str().unwrap_or_default().to_owned(),
            messages,
            busy: data["busy"].as_bool().unwrap_or(false),
            scroll,
            returns_to_chat,
        });
    }

    /// Show a provider thread switched to from a chat (F7, F8) while its transcript loads;
    /// Esc closes back to that chat.
    pub(crate) fn begin_thread(&mut self, project: String, member: String, title: String) {
        self.notice = Some("Loading…".into());
        self.mode = Mode::Thread(ThreadView {
            project,
            member,
            title,
            returns_to_chat: true,
            ..ThreadView::default()
        });
    }

    /// The open thread view's member, for its refresh.
    pub(crate) fn open_thread_member(&self) -> Option<(String, String)> {
        match &self.mode {
            Mode::Thread(thread) => Some((thread.project.clone(), thread.member.clone())),
            _ => None,
        }
    }

    /// Open or refresh the review page of a project factory from its `factory-show` reply.
    /// `explicit` is a load the user asked for; a background refresh keeps the reviewed digest.
    pub(crate) fn open_factory(&mut self, project: String, data: &Value, explicit: bool) {
        let text = |key: &str| data[key].as_str().unwrap_or_default().to_owned();
        let name = text("name");
        let (scroll, reviewed, selected_node, node_detail) = match &self.mode {
            Mode::Factory(open) if open.name == name => (
                open.scroll,
                (!explicit).then(|| open.reviewed.clone()),
                open.selected_node.clone(),
                open.node_detail.clone(),
            ),
            _ if !explicit => return,
            _ => {
                self.notice = None;
                (0, None, None, None)
            }
        };
        let reviewed = reviewed.unwrap_or_else(|| text("digest"));
        if explicit {
            self.reviewed
                .insert(format!("{project}/{name}"), reviewed.clone());
        }
        let graph = data.get("graph").filter(|g| g.is_object()).cloned();
        let default_node = selected_node.or_else(|| {
            let theme = Theme::builtin("prime", crate::theme::ColorMode::TrueColor);
            graph.as_ref().and_then(|spec| {
                crate::factory_view::machine_block_diagram(
                    &theme,
                    spec,
                    &crate::factory_view::DiagramRenderOptions::default(),
                )
                .and_then(|d| d.node_spans.first().map(|(id, _, _)| id.clone()))
            })
        });
        self.mode = Mode::Factory(FactoryReview {
            project,
            description: text("description"),
            params: data["params"].as_array().cloned().unwrap_or_default(),
            state_details: data
                .get("state_details")
                .cloned()
                .unwrap_or_else(|| json!({})),
            state: text("state"),
            approved: data["approved"].as_bool().unwrap_or(false),
            path: text("path"),
            file: text("file"),
            digest: text("digest"),
            reviewed,
            errors: data["errors"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            graph,
            graph_error: data["graph_error"].as_str().map(str::to_owned),
            live: data["live"].as_bool().unwrap_or(false),
            modules: data["modules"].as_array().cloned().unwrap_or_default(),
            specialists: data["specialists"].as_array().cloned().unwrap_or_default(),
            name,
            scroll,
            manual_scroll: false,
            selected_node: default_node,
            node_detail,
            inherit_model: data["inherit_model"]["model"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            default_role: data["default_role"]["model"].as_str().map(|model| {
                (
                    model.to_owned(),
                    data["default_role"]["thinking"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                )
            }),
        });
    }

    /// Show the `edit-history` reply of one Captain file.
    pub(crate) fn open_history(&mut self, project: String, field: String, data: &Value) {
        let edits = data["edits"].as_array().cloned().unwrap_or_default();
        self.notice = edits
            .is_empty()
            .then(|| format!("No edits of {} yet.", field.replace('_', " ")));
        self.mode = Mode::History(HistoryView {
            project,
            field,
            edits,
            selected: 0,
        });
    }

    /// The open factory review page, for its refresh.
    pub(crate) fn open_factory_member(&self) -> Option<(String, String)> {
        match &self.mode {
            Mode::Factory(review) => Some((review.project.clone(), review.name.clone())),
            _ => None,
        }
    }

    pub(crate) fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> CaptainAction {
        let mode = std::mem::replace(&mut self.mode, Mode::Browse);
        let (mode, action) = match mode {
            Mode::Browse => (Mode::Browse, self.browse_key(key, kb)),
            Mode::Pick(picker) => self.picker_key(picker, key, kb),
            Mode::Confirm(confirm) => self.confirm_key(confirm, key, kb),
            Mode::Thread(thread) => self.thread_key(thread, key, kb),
            Mode::Factory(review) => self.factory_key(review, key, kb),
            Mode::Compose(compose) => self.compose_key(compose, key, kb),
            Mode::Specialist(view) => self.specialist_key(view, key, kb),
            Mode::History(history) => self.history_key(history, key, kb),
        };
        // An action may already have opened a new mode (a setup step).
        if matches!(self.mode, Mode::Browse) {
            self.mode = mode;
        }
        action
    }

    fn browse_key(&mut self, key: &str, kb: &KeybindingsManager) -> CaptainAction {
        self.notice = None;
        self.error = None;
        if kb.matches(key, "app.factory.toggle") || key == "q" || key == "ctrl+c" {
            return CaptainAction::Close;
        }
        if self.inbox.is_some() && self.tab == Tab::Overview {
            return self.inbox_key(key, kb);
        }
        if let Some(drawer) = self.drawer.filter(|_| self.tab == Tab::Overview) {
            return self.drawer_key(drawer, key, kb);
        }
        if self.tab == Tab::Overview {
            if let Some(item) = self.selected_need().filter(|i| i.kind == "decision") {
                if let Some(option) = item.options.iter().find(|o| o.key == key) {
                    let project = self.selected_project().map(|p| p.id.clone());
                    return CaptainAction::Act {
                        op: "decision-resolve".into(),
                        args: json!({
                            "project": project,
                            "id": item.decision_id.clone().unwrap_or_else(|| item.id.clone()),
                            "key": option.key,
                        }),
                        done: "Decision sent to Captain.".into(),
                    };
                }
            }
            if self.selected_project().is_some() {
                if key == "w" {
                    self.drawer = Some(Drawer::Wake(0));
                    self.help = false;
                    return CaptainAction::None;
                }
                let has_schedules = self
                    .selected_project()
                    .is_some_and(|p| !p.schedules.is_empty());
                if matches!(key, "S" | "shift+s") && has_schedules {
                    self.drawer = Some(Drawer::Schedules(0));
                    self.help = false;
                    return CaptainAction::None;
                }
            }
        }
        if kb.matches(key, "tui.select.cancel") {
            if self.help {
                self.help = false;
                return CaptainAction::None;
            }
            return CaptainAction::Close;
        }
        if key == "?" {
            self.help = !self.help;
            return CaptainAction::None;
        }
        if let Some(tab) = match key {
            "1" | "2" | "3" | "4" => key.parse::<usize>().ok().map(|n| Tab::ALL[n - 1]),
            "[" => Some(Tab::ALL[(self.tab.index() + 3) % 4]),
            "]" => Some(Tab::ALL[(self.tab.index() + 1) % 4]),
            _ => None,
        } {
            self.set_tab(tab);
            return CaptainAction::None;
        }
        if key == "i" && self.tab == Tab::Overview && self.selected_project().is_some() {
            self.inbox = Some(0);
            self.help = false;
            return CaptainAction::None;
        }
        if self.tab == Tab::Overview && self.focus == Focus::Crew {
            if let Some(action) = self.step_key(key, kb) {
                return action;
            }
        }
        if self.tab != Tab::Overview {
            if let Some(action) = self.tab_key(key, kb) {
                return action;
            }
        } else if let Some(item) = self.selected_need() {
            if let Some(action) = self.need_key(&item, key, kb) {
                return action;
            }
        }
        match key {
            "r" => return CaptainAction::Refresh,
            "tab" | "shift+tab" if self.tab == Tab::Overview => {
                let has_rows = !self.entries().is_empty();
                self.focus = match self.focus {
                    Focus::Projects if has_rows => Focus::Crew,
                    _ => Focus::Projects,
                };
                return CaptainAction::None;
            }
            "n" if !(self.focus == Focus::Crew && self.tab == Tab::Overview) => {
                return self.start_setup(None)
            }
            "p" => return self.start_setup(Some("current".into())),
            _ => {}
        }
        if (key == "right" || key == "l") && self.tab == Tab::Overview {
            if let Some(member) = self.selected_member() {
                if self.subagent_count(&member) > 0 {
                    if let Some(session) = member.session.clone() {
                        return CaptainAction::OpenSubagents { session };
                    }
                }
            }
        }
        if kb.matches(key, "tui.select.up") || key == "k" {
            self.move_selection(-1);
            return CaptainAction::None;
        }
        if kb.matches(key, "tui.select.down") || key == "j" {
            self.move_selection(1);
            return CaptainAction::None;
        }
        if key == "left" || key == "h" {
            self.focus = Focus::Projects;
            self.move_selection(-1);
            return CaptainAction::None;
        }
        if key == "right" || key == "l" {
            self.focus = Focus::Projects;
            self.move_selection(1);
            return CaptainAction::None;
        }
        let Some(project) = self.selected_project().cloned() else {
            if key == "f" {
                return CaptainAction::Act {
                    op: "factory".into(),
                    args: json!({}),
                    done: String::new(),
                };
            }
            return CaptainAction::None;
        };
        let member = self.selected_member();
        let pid = project.id.clone();
        match key {
            "A" | "shift+a" => match &member {
                Some(m) if !m.is_run() => {
                    let op = if m.archived { "unarchive" } else { "archive" };
                    CaptainAction::Act {
                        op: op.into(),
                        args: json!({"project": pid, "member": m.id}),
                        done: format!("{} {op}d.", m.label()),
                    }
                }
                Some(_) => {
                    self.notice = Some("Archive applies to crew threads.".into());
                    CaptainAction::None
                }
                None => {
                    self.show_archived = !self.show_archived;
                    self.clamp_member();
                    CaptainAction::None
                }
            },
            "s" => {
                if !project.is_active() {
                    self.notice = Some(format!("This Captain is {}.", project.display_state()));
                    return CaptainAction::None;
                }
                self.mode = Mode::Confirm(Confirm::Stop {
                    project: pid,
                    name: project.name.clone(),
                });
                CaptainAction::None
            }
            "R" => CaptainAction::Act {
                op: "resume".into(),
                args: json!({"project": pid}),
                done: "Captain resumed.".into(),
            },
            "D" => {
                let running = usize::from(project.lead_busy)
                    + project
                        .crew
                        .iter()
                        .filter(|m| m.busy || m.state == "running")
                        .count();
                self.mode = Mode::Confirm(Confirm::Delete {
                    project: pid,
                    name: project.name.clone(),
                    crew: project.crew.iter().filter(|m| !m.is_run()).count(),
                    running,
                });
                CaptainAction::None
            }
            "e" => {
                let items = [
                    "objective",
                    "plan",
                    "rules",
                    "crew_rules",
                    "digest",
                    "name",
                    "model",
                    "thinking",
                    "parallel",
                ]
                .iter()
                .map(|field| PickItem {
                    label: field.replace('_', " "),
                    value: json!(field),
                })
                .collect();
                self.mode = Mode::Pick(Picker::new(
                    "edit",
                    items,
                    PickPurpose::EditField { project: pid },
                ));
                CaptainAction::None
            }
            "d" => {
                if !project.is_active() {
                    self.notice = Some("Resume this Captain to assign work.".into());
                    return CaptainAction::None;
                }
                self.mode = Mode::Pick(Picker::new(
                    "assign work",
                    Vec::new(),
                    PickPurpose::Delegate {
                        project: pid.clone(),
                    },
                ));
                CaptainAction::LoadChoices {
                    project: Some(pid),
                    reference: None,
                }
            }
            "n" | "N" | "shift+n" => new_thread_action(pid),
            "a" => self.start_adopt(pid),
            "M" | "shift+m" => match member {
                Some(m) if !m.is_run() => CaptainAction::PickModel(CaptainModelTarget::Crew {
                    project: pid,
                    member: m.id.clone(),
                    label: m.label(),
                    current_model: m.model.clone(),
                    current_thinking: m.thinking.clone(),
                }),
                Some(_) => {
                    self.notice = Some("Select a crew thread to change its model.".into());
                    CaptainAction::None
                }
                None => CaptainAction::PickModel(CaptainModelTarget::CaptainLead {
                    project: pid,
                    current_model: project.model.clone(),
                    current_thinking: project.thinking.clone(),
                }),
            },
            "m" => match &member {
                Some(m) if m.is_run() => {
                    if m.run_state.as_deref() == Some("paused") && m.headless {
                        CaptainAction::Compose {
                            purpose: ComposePurpose::Answer {
                                project: pid,
                                member: m.id.clone(),
                            },
                            label: format!("answer {} · the run resumes with this note", m.label()),
                            initial: String::new(),
                        }
                    } else {
                        self.notice = Some("Only a paused run takes an answer.".into());
                        CaptainAction::None
                    }
                }
                _ => {
                    if !project.is_active() {
                        self.notice = Some("Resume this Captain to send messages.".into());
                        return CaptainAction::None;
                    }
                    let to = member
                        .as_ref()
                        .map_or("captain".into(), CaptainMember::label);
                    CaptainAction::Compose {
                        purpose: ComposePurpose::Message {
                            project: pid,
                            member: member.as_ref().map(|m| m.id.clone()),
                        },
                        label: format!("message {to}"),
                        initial: String::new(),
                    }
                }
            },
            "F" | "shift+f" => {
                let mut items: Vec<PickItem> = project
                    .factories
                    .iter()
                    .map(|f| PickItem {
                        label: format!(
                            "{} · {}",
                            clean(&f.name),
                            factory_state_label(&f.state, f.approved)
                        ),
                        value: json!({"name": f.name}),
                    })
                    .collect();
                items.push(PickItem {
                    label: "Design a factory for this project".into(),
                    value: json!({"design": true}),
                });
                self.mode = Mode::Pick(Picker::new(
                    "project factories",
                    items,
                    PickPurpose::Factories {
                        project: pid,
                        request: uuid::Uuid::new_v4().to_string(),
                    },
                ));
                CaptainAction::None
            }
            "S" | "shift+s" => {
                if project.specialists.is_empty() {
                    self.notice = Some("No specialists yet. The Captain writes them.".into());
                    return CaptainAction::None;
                }
                let items = project
                    .specialists
                    .iter()
                    .map(|s| PickItem {
                        label: specialist_label(s),
                        value: serde_json::to_value(s).unwrap_or(Value::Null),
                    })
                    .collect();
                self.mode = Mode::Pick(Picker::new(
                    "specialists",
                    items,
                    PickPurpose::Specialists { project: pid },
                ));
                CaptainAction::None
            }
            "c" => CaptainAction::Compose {
                purpose: ComposePurpose::Consensus { project: pid },
                label: "consensus · the decision both models should vote on".into(),
                initial: String::new(),
            },
            "f" => CaptainAction::Act {
                op: "factory".into(),
                args: json!({"project": pid}),
                done: String::new(),
            },
            "u" => match member {
                Some(m) if !m.is_run() => {
                    self.mode = Mode::Confirm(Confirm::Release {
                        project: pid,
                        member: m.id.clone(),
                        title: m.label(),
                    });
                    CaptainAction::None
                }
                _ => CaptainAction::None,
            },
            "v" => match member {
                Some(m) if m.kind == "thread" => CaptainAction::Review {
                    project: pid,
                    member: m.id,
                },
                _ => {
                    self.notice = Some("Select a crew thread to review its checkout.".into());
                    CaptainAction::None
                }
            },
            "o" | "enter" => self.open_selected(&project, member.as_ref()),
            _ if kb.matches(key, "tui.select.confirm") => {
                self.open_selected(&project, member.as_ref())
            }
            _ => CaptainAction::None,
        }
    }

    fn open_selected(
        &mut self,
        project: &CaptainProject,
        member: Option<&CaptainMember>,
    ) -> CaptainAction {
        if let Some(m) = member {
            self.mark_seen(&m.id);
        }
        if let Some(m) = member.filter(|m| m.is_run() && m.headless && !m.run_ended()) {
            // A live Captain-started run has no chat; the live factory page shows it.
            return CaptainAction::OpenFactoryRun {
                project: project.id.clone(),
                member: m.id.clone(),
                run_id: m.run_id.clone(),
            };
        }
        CaptainAction::Open {
            project: project.id.clone(),
            member: member.map(|m| m.id.clone()),
        }
    }

    /// A selected run or step row: Enter on a step enters its live subagent; `l` and `f`
    /// open the run's live factory page.
    fn step_key(&mut self, key: &str, kb: &KeybindingsManager) -> Option<CaptainAction> {
        let project = self.selected_project()?.id.clone();
        let enter = key == "enter" || key == "o" || kb.matches(key, "tui.select.confirm");
        if enter {
            let agent = self.selected_step()?;
            if let Some(member) = self.selected_member() {
                self.mark_seen(&member.id);
            }
            return Some(step_open_action(&project, &agent));
        }
        if matches!(key, "l" | "f") {
            let member = self
                .selected_member()
                .filter(|m| m.is_run() && m.headless)?;
            self.mark_seen(&member.id);
            return Some(CaptainAction::OpenFactoryRun {
                project,
                member: member.id,
                run_id: member.run_id,
            });
        }
        None
    }

    /// The Notifications Inbox: ↑↓ move, Enter opens the member a notification names, `r`
    /// marks all read, `i` or Esc goes back to the Overview.
    fn inbox_key(&mut self, key: &str, kb: &KeybindingsManager) -> CaptainAction {
        let Some(project) = self.selected_project().cloned() else {
            self.inbox = None;
            return CaptainAction::None;
        };
        let notes = project.inbox();
        let selected = self.inbox.unwrap_or(0).min(notes.len().saturating_sub(1));
        if key == "i" || kb.matches(key, "tui.select.cancel") {
            self.inbox = None;
            return CaptainAction::None;
        }
        if kb.matches(key, "tui.select.up") || key == "k" {
            self.inbox = Some(selected.saturating_sub(1));
            return CaptainAction::None;
        }
        if kb.matches(key, "tui.select.down") || key == "j" {
            self.inbox = Some((selected + 1).min(notes.len().saturating_sub(1)));
            return CaptainAction::None;
        }
        if key == "r" {
            if let Some(p) = self.snapshot.projects.get_mut(self.project) {
                for note in p
                    .notifications
                    .iter_mut()
                    .chain(&mut p.unread_notifications)
                {
                    note.read = true;
                }
            }
            return CaptainAction::Act {
                op: "notifications-read".into(),
                args: json!({"project": project.id}),
                done: "Marked all notifications read.".into(),
            };
        }
        if key == "enter" || key == "o" || kb.matches(key, "tui.select.confirm") {
            let member = notes
                .get(selected)
                .map(|n| n.member.clone())
                .filter(|m| !m.is_empty());
            if let Some(m) = &member {
                self.mark_seen(m);
            }
            self.inbox = None;
            return match member.and_then(|m| project.crew.iter().find(|c| c.id == m).cloned()) {
                Some(m) => self.open_selected(&project, Some(&m)),
                None => CaptainAction::Open {
                    project: project.id.clone(),
                    member: None,
                },
            };
        }
        CaptainAction::None
    }

    /// A key in the Wake Policies or Schedules drawer.
    fn drawer_key(&mut self, drawer: Drawer, key: &str, kb: &KeybindingsManager) -> CaptainAction {
        let Some(project) = self.selected_project().cloned() else {
            self.drawer = None;
            return CaptainAction::None;
        };
        let (selected, count) = match drawer {
            Drawer::Wake(row) => (row, WAKE_EVENTS.len()),
            Drawer::Schedules(row) => (row, project.schedules.len()),
        };
        let selected = selected.min(count.saturating_sub(1));
        let moved = |row: usize| match drawer {
            Drawer::Wake(_) => Drawer::Wake(row),
            Drawer::Schedules(_) => Drawer::Schedules(row),
        };
        let toggle_key = match drawer {
            Drawer::Wake(_) => key == "w",
            Drawer::Schedules(_) => matches!(key, "S" | "shift+s"),
        };
        if toggle_key || kb.matches(key, "tui.select.cancel") {
            self.drawer = None;
            return CaptainAction::None;
        }
        if kb.matches(key, "tui.select.up") || key == "k" {
            self.drawer = Some(moved(selected.saturating_sub(1)));
            return CaptainAction::None;
        }
        if kb.matches(key, "tui.select.down") || key == "j" {
            self.drawer = Some(moved((selected + 1).min(count.saturating_sub(1))));
            return CaptainAction::None;
        }
        let activate = key == "space" || key == " " || key == "enter";
        match drawer {
            Drawer::Wake(_) if activate => {
                let event = WAKE_EVENTS[selected];
                let current = project.wake_policy.get(event).and_then(Value::as_str);
                let next = next_wake_policy(current.unwrap_or("react"));
                CaptainAction::Act {
                    op: "wake-policy".into(),
                    args: json!({"project": project.id, "policy": {event: next}}),
                    done: "Updated wake policy.".into(),
                }
            }
            Drawer::Schedules(_) => {
                let Some(schedule) = project.schedules.get(selected) else {
                    return CaptainAction::None;
                };
                match key {
                    _ if key == "space" || key == " " => CaptainAction::Act {
                        op: "schedule-toggle".into(),
                        args: json!({
                            "project": project.id,
                            "id": schedule.id,
                            "enabled": !schedule.enabled,
                        }),
                        done: if schedule.enabled {
                            format!("Paused {}.", clean(&schedule.title))
                        } else {
                            format!("Enabled {}.", clean(&schedule.title))
                        },
                    },
                    "x" => CaptainAction::Act {
                        op: "schedule-delete".into(),
                        args: json!({"project": project.id, "id": schedule.id}),
                        done: format!("Deleted {}.", clean(&schedule.title)),
                    },
                    _ => CaptainAction::None,
                }
            }
            Drawer::Wake(_) => CaptainAction::None,
        }
    }

    fn set_tab(&mut self, tab: Tab) {
        if tab == Tab::Factories && self.tab != Tab::Factories {
            self.design_request = uuid::Uuid::new_v4().to_string();
        }
        self.tab = tab;
        self.help = false;
    }

    /// The selected project's Drive: the Captain's files, then its specialists.
    fn drive_files(&self) -> Vec<DriveFile> {
        let mut files: Vec<DriveFile> = DRIVE_FILES
            .iter()
            .map(|&(name, field)| DriveFile::Field { name, field })
            .collect();
        if let Some(project) = self.selected_project() {
            files.extend(
                project
                    .specialists
                    .iter()
                    .cloned()
                    .map(DriveFile::Specialist),
            );
            let listed = &project.drive_files;
            for (dir, names) in [("reports", &listed.reports), ("handoffs", &listed.handoffs)] {
                let names = if names.is_empty() {
                    saved_names(&project.root, dir)
                } else {
                    names.clone()
                };
                files.extend(names.into_iter().map(|name| DriveFile::Saved { dir, name }));
            }
        }
        files
    }

    /// `a` on a factory in a list: approve the version the user last reviewed, else open its
    /// review page first.
    fn approve_or_review(&mut self, project: &str, name: &str, digest: &str) -> CaptainAction {
        let reviewed = self.reviewed.get(&format!("{project}/{name}"));
        if !digest.is_empty() && reviewed.map(String::as_str) == Some(digest) {
            return CaptainAction::Act {
                op: "factory-approve".into(),
                args: json!({"project": project, "name": name, "digest": digest}),
                done: format!("Approved this version of {name}."),
            };
        }
        self.notice = Some(format!("Review {name}, then press a to approve."));
        CaptainAction::LoadFactory {
            project: project.into(),
            name: name.into(),
        }
    }

    /// A key on a selected needs-you item; `None` falls through to the browse keys.
    fn need_key(
        &mut self,
        item: &NeedsYouItem,
        key: &str,
        kb: &KeybindingsManager,
    ) -> Option<CaptainAction> {
        let project = self.selected_project()?.id.clone();
        let enter = key == "enter" || key == "o" || kb.matches(key, "tui.select.confirm");
        let member = item.member.clone().unwrap_or_default();
        match item.kind.as_str() {
            "draft_factory" | "changed_factory" => {
                let name = item.factory.clone()?;
                if enter {
                    return Some(CaptainAction::LoadFactory { project, name });
                }
                if key == "a" {
                    let digest = self
                        .selected_project()?
                        .factories
                        .iter()
                        .find(|f| f.name == name)
                        .map_or_else(|| item.digest.clone(), |f| f.digest.clone());
                    return Some(self.approve_or_review(&project, &name, &digest));
                }
                None
            }
            "pending_grant" => {
                let grant = item.grant_id.clone()?;
                let machine = item.machine.clone().unwrap_or_else(|| item.label.clone());
                match key {
                    "a" | "g" => Some(CaptainAction::Act {
                        op: "grant-approve".into(),
                        args: json!({"project": project, "grant_id": grant, "standing": key == "g"}),
                        done: if key == "g" {
                            format!("Granted {machine}. The run starts.")
                        } else {
                            format!("Approved {machine} once. The run starts.")
                        },
                    }),
                    "x" => Some(CaptainAction::Act {
                        op: "grant-decline".into(),
                        args: json!({"project": project, "grant_id": grant}),
                        done: format!("Declined {machine}."),
                    }),
                    _ if enter => {
                        self.notice = Some(format!("{machine} · {}", item.hint()));
                        Some(CaptainAction::None)
                    }
                    _ => None,
                }
            }
            "paused_run" => {
                let run_id = self
                    .selected_project()?
                    .crew
                    .iter()
                    .find(|m| m.id == member)
                    .and_then(|m| m.run_id.clone());
                match key {
                    _ if enter => Some(CaptainAction::OpenFactoryRun {
                        project,
                        member,
                        run_id,
                    }),
                    "r" => Some(CaptainAction::Act {
                        op: "answer".into(),
                        args: json!({"project": project, "member": member, "decision": "resume"}),
                        done: "Retrying. The run resumes.".into(),
                    }),
                    "s" => Some(CaptainAction::Act {
                        op: "answer".into(),
                        args: json!({"project": project, "member": member, "decision": "stop"}),
                        done: "Stopping the run.".into(),
                    }),
                    "m" => Some(CaptainAction::Compose {
                        purpose: ComposePurpose::Answer { project, member },
                        label: format!("answer {} · the run resumes with this note", item.label),
                        initial: String::new(),
                    }),
                    _ => None,
                }
            }
            "decision" if key == "m" => Some(CaptainAction::Compose {
                purpose: ComposePurpose::Message {
                    project,
                    member: None,
                },
                label: format!("reply to captain · {}", clean(&item.title)),
                initial: String::new(),
            }),
            "budget_exceeded" => match key {
                "b" => Some(CaptainAction::Act {
                    op: "budget".into(),
                    args: json!({"project": project, "extend_usd": 5.0}),
                    done: "Extended budget by +$5.".into(),
                }),
                "c" => Some(CaptainAction::Act {
                    op: "budget".into(),
                    args: json!({"project": project, "clear": true}),
                    done: "Cleared budget cap.".into(),
                }),
                _ => None,
            },
            "worktree_missing" if key == "r" => Some(CaptainAction::Act {
                op: "worktree-recreate".into(),
                args: json!({"project": project, "member": member}),
                done: format!("Recreated the worktree of {}.", item.label),
            }),
            _ => None,
        }
    }

    /// A key on the Digest, Drive, or Factories tab; `None` falls through to the browse keys.
    fn tab_key(&mut self, key: &str, kb: &KeybindingsManager) -> Option<CaptainAction> {
        let project = self.selected_project()?.clone();
        let page = (self.viewport_rows / 2).max(1);
        let up = kb.matches(key, "tui.select.up") || key == "k";
        let down = kb.matches(key, "tui.select.down") || key == "j";
        let enter = key == "enter" || kb.matches(key, "tui.select.confirm");
        let scroll = |value: &mut usize| -> bool {
            match key {
                _ if up => *value = value.saturating_sub(1),
                _ if down => *value += 1,
                "pageup" => *value = value.saturating_sub(page),
                "pagedown" => *value += page,
                _ => return false,
            }
            true
        };
        let edit_field = |field: &str| {
            let initial = match field {
                "rules" => project.rules.clone(),
                "crew_rules" => project.crew_rules.clone(),
                "plan" => project.plan.clone(),
                _ => project.digest.clone(),
            };
            CaptainAction::Compose {
                label: format!("edit {}", field.replace('_', " ")),
                purpose: ComposePurpose::EditText {
                    project: project.id.clone(),
                    field: field.into(),
                },
                initial,
            }
        };
        match self.tab {
            Tab::Overview => None,
            Tab::Digest => {
                if scroll(&mut self.digest_scroll) {
                    return Some(CaptainAction::None);
                }
                match key {
                    "e" => Some(edit_field("digest")),
                    "h" => Some(CaptainAction::LoadHistory {
                        project: project.id.clone(),
                        field: "digest".into(),
                    }),
                    _ => None,
                }
            }
            Tab::Drive => {
                let files = self.drive_files();
                if up || down {
                    self.drive = if up {
                        self.drive.saturating_sub(1)
                    } else {
                        (self.drive + 1).min(files.len().saturating_sub(1))
                    };
                    self.drive_scroll = 0;
                    return Some(CaptainAction::None);
                }
                if scroll(&mut self.drive_scroll) {
                    return Some(CaptainAction::None);
                }
                match (files.get(self.drive)?.clone(), key) {
                    (DriveFile::Field { field, .. }, "e") => Some(edit_field(field)),
                    (DriveFile::Field { field, .. }, "h") => Some(CaptainAction::LoadHistory {
                        project: project.id.clone(),
                        field: field.into(),
                    }),
                    (DriveFile::Specialist(specialist), _) if enter || key == "e" => {
                        let view = SpecialistView {
                            project: project.id.clone(),
                            specialist,
                            scroll: 0,
                        };
                        if key == "e" {
                            self.mode = Mode::Pick(Self::specialist_field_picker(view));
                        } else {
                            self.mode = Mode::Specialist(view);
                        }
                        Some(CaptainAction::None)
                    }
                    (DriveFile::Specialist(_) | DriveFile::Saved { .. }, "h") => {
                        self.notice = Some("History covers the Captain's own files.".into());
                        Some(CaptainAction::None)
                    }
                    (DriveFile::Saved { .. }, "e") => {
                        self.notice = Some("Crew reports and handoffs are read-only.".into());
                        Some(CaptainAction::None)
                    }
                    _ => None,
                }
            }
            Tab::Factories => {
                let count = project.factories.len();
                if up || down {
                    self.factory = if up {
                        self.factory.saturating_sub(1)
                    } else {
                        (self.factory + 1).min(count.saturating_sub(1))
                    };
                    return Some(CaptainAction::None);
                }
                if key == "n" {
                    if self.design_request.is_empty() {
                        self.design_request = uuid::Uuid::new_v4().to_string();
                    }
                    return Some(CaptainAction::Act {
                        op: "factory-design".into(),
                        args: json!({"project": project.id, "request_id": self.design_request}),
                        done: "Design requested. The Captain writes a draft for review.".into(),
                    });
                }
                if key == "b" {
                    let taken = |name: &str| project.factories.iter().any(|f| f.name == name);
                    let name = std::iter::once("pipeline".to_owned())
                        .chain((2..).map(|n| format!("pipeline-{n}")))
                        .find(|name| !taken(name))
                        .unwrap_or_default();
                    self.mode = Mode::Compose(ComposeView::new(
                        project.id.clone(),
                        name,
                        None,
                        &Value::Null,
                    ));
                    return Some(CaptainAction::None);
                }
                let factory = project.factories.get(self.factory)?.clone();
                match key {
                    _ if enter => Some(CaptainAction::LoadFactory {
                        project: project.id.clone(),
                        name: factory.name,
                    }),
                    "a" => {
                        Some(self.approve_or_review(&project.id, &factory.name, &factory.digest))
                    }
                    "D" | "shift+d" => {
                        self.mode = Mode::Confirm(Confirm::DeleteListedFactory {
                            project: project.id.clone(),
                            name: factory.name,
                            path: factory.path,
                        });
                        Some(CaptainAction::None)
                    }
                    _ => None,
                }
            }
        }
    }

    fn specialist_field_picker(view: SpecialistView) -> Picker {
        let items = ["prompt", "description"]
            .iter()
            .map(|field| PickItem {
                label: format!("edit {field}"),
                value: json!(field),
            })
            .collect();
        Picker::new(
            &format!("specialist {}", clean(&view.specialist.id)),
            items,
            PickPurpose::SpecialistField(Box::new(view)),
        )
    }

    fn move_selection(&mut self, delta: isize) {
        match self.focus {
            Focus::Projects => {
                let count = self.snapshot.projects.len();
                if count == 0 {
                    return;
                }
                self.project =
                    (self.project as isize + delta).clamp(0, count as isize - 1) as usize;
                self.member = 0;
                self.digest_scroll = 0;
                self.drive = 0;
                self.drive_scroll = 0;
                self.factory = 0;
            }
            Focus::Crew => {
                let count = self.entries().len();
                if count == 0 {
                    return;
                }
                self.member = (self.member as isize + delta).clamp(0, count as isize - 1) as usize;
            }
        }
    }

    fn picker_key(
        &mut self,
        mut picker: Picker,
        key: &str,
        kb: &KeybindingsManager,
    ) -> (Mode, CaptainAction) {
        if kb.matches(key, "tui.select.cancel") || key == "ctrl+c" {
            match picker.purpose {
                PickPurpose::Launch => return (Mode::Browse, CaptainAction::Close),
                PickPurpose::RoleOrState { review, .. } => {
                    return (Mode::Factory(*review), CaptainAction::None)
                }
                PickPurpose::SpecialistField(view) => {
                    return (Mode::Specialist(*view), CaptainAction::None)
                }
                PickPurpose::AddStage(review) => {
                    return (Mode::Factory(*review), CaptainAction::None)
                }
                _ => {}
            }
            if let PickPurpose::Setup(setup) = &picker.purpose {
                let mut setup = (**setup).clone();
                if setup.step > 0 {
                    setup.step = if setup.step == 3 {
                        1
                    } else {
                        setup.next_step(false)
                    };
                    let action = self.setup_step(setup);
                    return (std::mem::replace(&mut self.mode, Mode::Browse), action);
                }
            }
            self.notice = Some("Cancelled.".into());
            return (Mode::Browse, CaptainAction::None);
        }
        let visible = picker.visible();
        if kb.matches(key, "tui.select.up") {
            picker.selected = picker.selected.saturating_sub(1);
            return (Mode::Pick(picker), CaptainAction::None);
        }
        if kb.matches(key, "tui.select.down") {
            picker.selected = (picker.selected + 1).min(visible.len().saturating_sub(1));
            return (Mode::Pick(picker), CaptainAction::None);
        }
        if picker.multi && key == "space" {
            if let Some(index) = picker.current() {
                if let Some(at) = picker.marked.iter().position(|&m| m == index) {
                    picker.marked.remove(at);
                } else {
                    picker.marked.push(index);
                }
            }
            return (Mode::Pick(picker), CaptainAction::None);
        }
        if key == "backspace" {
            picker.filter.pop();
            picker.selected = 0;
            return (Mode::Pick(picker), CaptainAction::None);
        }
        if kb.matches(key, "tui.select.confirm") || key == "enter" {
            let value = picker.current().map(|i| picker.items[i].value.clone());
            if value.is_none() && !picker.multi {
                return (Mode::Pick(picker), CaptainAction::None);
            }
            let value = value.unwrap_or(Value::Null);
            let action = self.pick(&picker, &value);
            return (std::mem::replace(&mut self.mode, Mode::Browse), action);
        }
        let mut chars = key.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if !c.is_control() {
                picker.filter.push(c);
                picker.selected = 0;
            }
        } else if let Some(rest) = key.strip_prefix("shift+") {
            if rest.chars().count() == 1 {
                picker.filter.push_str(&rest.to_uppercase());
                picker.selected = 0;
            }
        }
        (Mode::Pick(picker), CaptainAction::None)
    }

    /// The picker's Enter.
    fn pick(&mut self, picker: &Picker, value: &Value) -> CaptainAction {
        match picker.purpose.clone() {
            PickPurpose::EditField { project } => {
                let field = value.as_str().unwrap_or_default().to_owned();
                match field.as_str() {
                    "model" => {
                        let (current_model, current_thinking) = self
                            .selected_project()
                            .map(|p| (p.model.clone(), p.thinking.clone()))
                            .unwrap_or_default();
                        CaptainAction::PickModel(CaptainModelTarget::CaptainLead {
                            project,
                            current_model,
                            current_thinking,
                        })
                    }
                    "thinking" => {
                        let items = [
                            "default", "off", "minimal", "low", "medium", "high", "xhigh", "max",
                        ]
                        .iter()
                        .map(|l| PickItem {
                            label: (*l).into(),
                            value: json!(l),
                        })
                        .collect();
                        let mut next = Picker::new(
                            "captain thinking",
                            items,
                            PickPurpose::Thinking { project },
                        );
                        if let Some(p) = self.selected_project() {
                            next.select_value(&json!(if p.thinking.is_empty() {
                                "default"
                            } else {
                                &p.thinking
                            }));
                        }
                        self.mode = Mode::Pick(next);
                        CaptainAction::None
                    }
                    "parallel" => {
                        let items = [1u32, 2, 3, 4, 6, 8, 12, 16]
                            .iter()
                            .map(|n| PickItem {
                                label: format!("{n} parallel crew turns"),
                                value: json!(n.to_string()),
                            })
                            .collect();
                        let mut next =
                            Picker::new("worker limit", items, PickPurpose::Workers { project });
                        if let Some(p) = self.selected_project() {
                            next.select_value(&json!(p.parallel.to_string()));
                        }
                        self.mode = Mode::Pick(next);
                        CaptainAction::None
                    }
                    _ => {
                        let initial = self
                            .selected_project()
                            .map(|p| match field.as_str() {
                                "objective" => p.objective.clone(),
                                "plan" => p.plan.clone(),
                                "rules" => p.rules.clone(),
                                "crew_rules" => p.crew_rules.clone(),
                                "digest" => p.digest.clone(),
                                "name" => p.name.clone(),
                                _ => String::new(),
                            })
                            .unwrap_or_default();
                        CaptainAction::Compose {
                            label: format!("edit {}", field.replace('_', " ")),
                            purpose: ComposePurpose::EditText { project, field },
                            initial,
                        }
                    }
                }
            }
            PickPurpose::Thinking { project } => CaptainAction::Act {
                op: "edit".into(),
                args: json!({"project": project, "field": "thinking", "value": value}),
                done: "Captain thinking updated.".into(),
            },
            PickPurpose::Workers { project } => CaptainAction::Act {
                op: "edit".into(),
                args: json!({"project": project, "field": "parallel", "value": value}),
                done: "Worker limit updated.".into(),
            },
            PickPurpose::Delegate { project } => {
                if value.get("pick_model").and_then(Value::as_bool) == Some(true) {
                    return CaptainAction::PickModel(CaptainModelTarget::Assign { project });
                }
                if let Some(machine) = value.get("machine").and_then(Value::as_str) {
                    return CaptainAction::Compose {
                        label: format!("assignment for {machine}"),
                        purpose: ComposePurpose::DelegateTask {
                            project,
                            worker: Worker::Machine(machine.into()),
                        },
                        initial: String::new(),
                    };
                }
                let model = value["model"].as_str().unwrap_or_default().to_owned();
                self.apply_assign_model(project, model, String::new());
                CaptainAction::None
            }
            PickPurpose::DelegateWrite {
                project,
                model,
                thinking,
            } => CaptainAction::Compose {
                label: "assignment for the new crew thread".into(),
                purpose: ComposePurpose::DelegateTask {
                    project,
                    worker: Worker::Model {
                        model,
                        thinking,
                        write: value.as_bool().unwrap_or(false),
                    },
                },
                initial: String::new(),
            },
            PickPurpose::Adopt { project } => match value.as_str() {
                Some(reference) => CaptainAction::Act {
                    op: "adopt".into(),
                    args: json!({"project": project, "ref": reference}),
                    done: "Thread added to crew.".into(),
                },
                None => CaptainAction::Compose {
                    purpose: ComposePurpose::AdoptRef { project },
                    label: "thread ID or native session path".into(),
                    initial: String::new(),
                },
            },
            PickPurpose::Launch => {
                let machine = value.as_str().unwrap_or_default().to_owned();
                let params = self
                    .machine_params
                    .get(&machine)
                    .cloned()
                    .unwrap_or_default();
                if let Some(error) = params.iter().find_map(|p| p.error.clone()) {
                    self.error = Some(error);
                    return CaptainAction::None;
                }
                Self::launch_step(Launch {
                    machine,
                    params: params
                        .into_iter()
                        .map(|p| (p.name, p.kind, p.required, p.description))
                        .collect(),
                    index: 0,
                    values: Vec::new(),
                })
            }
            PickPurpose::Specialists { project } => {
                if let Ok(specialist) = serde_json::from_value(value.clone()) {
                    self.mode = Mode::Specialist(SpecialistView {
                        project,
                        specialist,
                        scroll: 0,
                    });
                }
                CaptainAction::None
            }
            PickPurpose::SpecialistField(view) => {
                let field = value.as_str().unwrap_or("prompt").to_owned();
                let initial = if field == "prompt" {
                    view.specialist.prompt.clone()
                } else {
                    view.specialist.description.clone()
                };
                let label = format!("specialist {} · {field}", clean(&view.specialist.id));
                let project = view.project.clone();
                let specialist = Box::new(view.specialist.clone());
                self.mode = Mode::Specialist(*view);
                CaptainAction::Compose {
                    purpose: ComposePurpose::SpecialistText {
                        project,
                        specialist,
                        field,
                    },
                    label,
                    initial,
                }
            }
            PickPurpose::RoleOrState {
                review,
                state_id,
                role,
                current_model,
                current_thinking,
            } => {
                let target = CaptainModelTarget::FactoryNode {
                    project: review.project.clone(),
                    name: review.name.clone(),
                    state_id,
                    role: (value.as_str() == Some("role")).then_some(role),
                    current_model,
                    current_thinking,
                };
                self.mode = Mode::Factory(*review);
                CaptainAction::PickModel(target)
            }
            PickPurpose::AddStage(review) => {
                let stage = value.as_str().unwrap_or_default().to_owned();
                let action = CaptainAction::SaveFactory {
                    op: "factory-add-stage".into(),
                    project: review.project.clone(),
                    name: review.name.clone(),
                    args: json!({"project": review.project, "name": review.name, "stage": stage}),
                };
                self.notice = Some(format!("Adding {stage}…"));
                self.mode = Mode::Factory(*review);
                action
            }
            PickPurpose::Factories { project, request } => {
                if value.get("design").is_some() {
                    return CaptainAction::Act {
                        op: "factory-design".into(),
                        args: json!({"project": project, "request_id": request}),
                        done: "Design requested. The Captain writes a draft for review.".into(),
                    };
                }
                let name = value["name"].as_str().unwrap_or_default().to_owned();
                self.notice = Some(format!("Loading {name}…"));
                CaptainAction::LoadFactory { project, name }
            }
            PickPurpose::Setup(setup) => {
                let mut setup = *setup;
                match SETUP_STEPS[setup.step] {
                    "model" => {
                        setup.provider = value[0].as_str().unwrap_or("prime").into();
                        setup.model = value[1].as_str().unwrap_or_default().into();
                    }
                    "thinking" => setup.thinking = value.as_str().unwrap_or_default().into(),
                    "factory" => {
                        if value.get("compose").is_some() {
                            let earlier = setup.compose.clone().unwrap_or_default();
                            let mut compose = ComposeView::new(
                                String::new(),
                                "pipeline".into(),
                                setup.compose.as_ref().map(|_| earlier.stages.as_slice()),
                                &earlier.models,
                            );
                            compose.setup = Some(Box::new(setup));
                            self.mode = Mode::Compose(compose);
                            return CaptainAction::None;
                        }
                        setup.factory_chosen = true;
                        setup.design = value.get("design").and_then(Value::as_bool) == Some(true);
                        setup.machine = value.as_str().map(str::to_owned);
                        setup.compose = None;
                    }
                    "factory models" => {
                        let mode = value.as_str().unwrap_or("captain");
                        if mode == "model" {
                            let current_model = setup.factory_model.clone();
                            let current_thinking = setup.factory_thinking.clone();
                            self.pending_setup = Some(setup);
                            return CaptainAction::PickModel(CaptainModelTarget::SetupFactory {
                                current_model,
                                current_thinking,
                            });
                        }
                        setup.factory_models = mode.into();
                    }
                    "workers" => setup.parallel = value.as_u64().unwrap_or(2) as u32,
                    "threads" => {
                        setup.crew = picker
                            .marked
                            .iter()
                            .filter_map(|&i| picker.items[i].value.as_str().map(str::to_owned))
                            .collect();
                    }
                    _ => {}
                }
                setup.step = setup.next_step(true);
                self.setup_step(setup)
            }
        }
    }

    fn confirm_key(
        &mut self,
        confirm: Confirm,
        key: &str,
        kb: &KeybindingsManager,
    ) -> (Mode, CaptainAction) {
        let yes = key == "y" || key == "enter" || kb.matches(key, "tui.select.confirm");
        let no = key == "n" || key == "ctrl+c" || kb.matches(key, "tui.select.cancel");
        if no {
            if let Confirm::DeleteFactory(review) = confirm {
                return (Mode::Factory(*review), CaptainAction::None);
            }
            if let Confirm::DeleteSpecialist(view) = confirm {
                return (Mode::Specialist(*view), CaptainAction::None);
            }
            if let Confirm::RemoveStage { review, .. } = confirm {
                return (Mode::Factory(*review), CaptainAction::None);
            }
            if let Confirm::Create(setup) = confirm {
                let mut setup = *setup;
                setup.step = SETUP_STEPS.len() - 2;
                let action = self.setup_step(setup);
                return (std::mem::replace(&mut self.mode, Mode::Browse), action);
            }
            self.notice = Some("Cancelled.".into());
            return (Mode::Browse, CaptainAction::None);
        }
        if !yes {
            return (Mode::Confirm(confirm), CaptainAction::None);
        }
        let action = match confirm {
            Confirm::Stop { project, .. } => CaptainAction::Act {
                op: "stop".into(),
                args: json!({"project": project}),
                done: "Captain stopped. Press R to resume.".into(),
            },
            Confirm::Delete { project, .. } => CaptainAction::Act {
                op: "delete".into(),
                args: json!({"project": project}),
                done: "Captain deleted. Its threads stay in the thread list.".into(),
            },
            Confirm::Release {
                project, member, ..
            } => CaptainAction::Act {
                op: "release".into(),
                args: json!({"project": project, "member": member}),
                done: "Thread released to the thread list.".into(),
            },
            Confirm::DeleteFactory(review) => CaptainAction::Act {
                op: "factory-delete".into(),
                args: json!({"project": review.project, "name": review.name}),
                done: format!("Deleted the factory {}.", review.name),
            },
            Confirm::DeleteListedFactory { project, name, .. } => CaptainAction::Act {
                op: "factory-delete".into(),
                args: json!({"project": project, "name": name}),
                done: format!("Deleted the factory {name}."),
            },
            Confirm::DeleteSpecialist(view) => CaptainAction::Act {
                op: "specialist-delete".into(),
                args: json!({"project": view.project, "id": view.specialist.id}),
                done: format!("Deleted the specialist {}.", clean(&view.specialist.id)),
            },
            Confirm::RemoveStage { review, stage } => {
                let action = CaptainAction::SaveFactory {
                    op: "factory-remove-stage".into(),
                    project: review.project.clone(),
                    name: review.name.clone(),
                    args: json!({"project": review.project, "name": review.name, "stage": stage}),
                };
                return (Mode::Factory(*review), action);
            }
            Confirm::Create(setup) => {
                let op = if setup.reference.is_some() {
                    "promote"
                } else {
                    "init"
                };
                CaptainAction::Act {
                    op: op.into(),
                    args: setup.request(),
                    done: if setup.design {
                        "Captain created. It writes its plan, then a factory draft for review."
                    } else {
                        "Captain created. It is writing its plan."
                    }
                    .into(),
                }
            }
        };
        (Mode::Browse, action)
    }

    fn thread_key(
        &mut self,
        mut thread: ThreadView,
        key: &str,
        kb: &KeybindingsManager,
    ) -> (Mode, CaptainAction) {
        if kb.matches(key, "tui.select.cancel") || key == "q" || key == "left" {
            if thread.returns_to_chat {
                return (Mode::Browse, CaptainAction::Close);
            }
            return (Mode::Browse, CaptainAction::Refresh);
        }
        if kb.matches(key, "tui.select.up") || key == "k" {
            thread.scroll += 1;
            return (Mode::Thread(thread), CaptainAction::None);
        }
        if kb.matches(key, "tui.select.down") || key == "j" {
            thread.scroll = thread.scroll.saturating_sub(1);
            return (Mode::Thread(thread), CaptainAction::None);
        }
        if key == "pageup" {
            thread.scroll += self.viewport_rows / 2;
            return (Mode::Thread(thread), CaptainAction::None);
        }
        if key == "pagedown" {
            thread.scroll = thread.scroll.saturating_sub(self.viewport_rows / 2);
            return (Mode::Thread(thread), CaptainAction::None);
        }
        if key == "r" || key == "m" || kb.matches(key, "tui.select.confirm") {
            if thread.busy {
                self.notice = Some(format!(
                    "{} is working. Reply when the turn ends.",
                    thread.title
                ));
                return (Mode::Thread(thread), CaptainAction::None);
            }
            let member = (thread.member != "lead").then(|| thread.member.clone());
            let action = CaptainAction::Compose {
                purpose: ComposePurpose::Message {
                    project: thread.project.clone(),
                    member,
                },
                label: format!("reply to {}", thread.title),
                initial: String::new(),
            };
            return (Mode::Thread(thread), action);
        }
        (Mode::Thread(thread), CaptainAction::None)
    }

    /// `specialist-write` with every field of `specialist`, so one change keeps the rest.
    fn specialist_write(project: &str, specialist: &ProjectSpecialist) -> CaptainAction {
        CaptainAction::Act {
            op: "specialist-write".into(),
            args: json!({
                "project": project,
                "id": specialist.id,
                "name": specialist.name,
                "description": specialist.description,
                "model": specialist.model,
                "thinking": specialist.thinking,
                "prompt": specialist.prompt,
            }),
            done: format!("Updated the specialist {}.", clean(&specialist.id)),
        }
    }

    /// The shared model picker returned a model for the open specialist.
    pub(crate) fn apply_specialist_model(
        &mut self,
        id: &str,
        model: String,
        effort_edited: bool,
        thinking: String,
    ) -> CaptainAction {
        let Mode::Specialist(view) = &mut self.mode else {
            return CaptainAction::None;
        };
        if view.specialist.id != id {
            return CaptainAction::None;
        }
        view.specialist.model = model;
        if effort_edited {
            view.specialist.thinking = thinking;
        }
        let (project, specialist) = (view.project.clone(), view.specialist.clone());
        Self::specialist_write(&project, &specialist)
    }

    fn history_key(
        &mut self,
        mut history: HistoryView,
        key: &str,
        kb: &KeybindingsManager,
    ) -> (Mode, CaptainAction) {
        if kb.matches(key, "tui.select.cancel") || key == "q" || key == "left" {
            return (Mode::Browse, CaptainAction::None);
        }
        if kb.matches(key, "tui.select.up") || key == "k" {
            history.selected = history.selected.saturating_sub(1);
        } else if kb.matches(key, "tui.select.down") || key == "j" {
            history.selected = (history.selected + 1).min(history.edits.len().saturating_sub(1));
        } else if key == "r" {
            let Some(edit) = history.edits.get(history.selected) else {
                return (Mode::History(history), CaptainAction::None);
            };
            if edit["restorable"].as_bool() != Some(true) {
                self.notice = Some("That edit kept no earlier text.".into());
                return (Mode::History(history), CaptainAction::None);
            }
            let field = history.field.replace('_', " ");
            return (
                Mode::Browse,
                CaptainAction::Act {
                    op: "edit-restore".into(),
                    args: json!({"project": history.project, "edit_id": edit["id"]}),
                    done: format!("Restored the {field} before that edit."),
                },
            );
        }
        (Mode::History(history), CaptainAction::None)
    }

    fn render_history(
        theme: &Theme,
        width: usize,
        history: &HistoryView,
        rows: usize,
    ) -> Vec<Line> {
        let file = DRIVE_FILES
            .iter()
            .find(|(_, field)| *field == history.field)
            .map_or(history.field.as_str(), |(name, _)| name);
        let mut out: Vec<Line> = vec![vec![
            Span::raw(" "),
            theme.fg_span(ThemeColor::Dim, "history › "),
            theme.bold(theme.fg_span(ThemeColor::Text, file.to_owned())),
        ]];
        let now = now_seconds();
        let room = rows.saturating_sub(1).max(1);
        let top = history.selected.saturating_sub(room - 1);
        for (index, edit) in history.edits.iter().enumerate().skip(top).take(room) {
            let age = edit["stamp"]
                .as_f64()
                .map_or_else(String::new, |s| age_label(now - s));
            let size = format!(
                "{} → {} chars",
                edit["old_len"].as_i64().unwrap_or(0),
                edit["new_len"].as_i64().unwrap_or(0)
            );
            let mut trailing = vec![MenuSegment::muted(&size)];
            if edit["restorable"].as_bool() == Some(true) {
                trailing.push(MenuSegment::themed(ThemeColor::Accent, "r restore"));
            }
            out.push(menu_row(
                theme,
                width,
                vec![
                    theme.fg_span(
                        ThemeColor::Text,
                        clean(edit["actor"].as_str().unwrap_or("?")),
                    ),
                    theme.fg_span(ThemeColor::Dim, format!("  {age}")),
                ],
                &trailing,
                index == history.selected,
            ));
        }
        out
    }

    fn specialist_key(
        &mut self,
        mut view: SpecialistView,
        key: &str,
        kb: &KeybindingsManager,
    ) -> (Mode, CaptainAction) {
        let page = (self.viewport_rows / 2).max(1);
        if kb.matches(key, "tui.select.cancel") || key == "q" || key == "left" {
            return (Mode::Browse, CaptainAction::None);
        }
        let whole = view.specialist.prompt.len() < SPECIALIST_PROMPT_SHOWN;
        match key {
            "m" | "e" if !whole => {
                self.notice =
                    Some("This prompt is longer than the panel holds. Edit its file.".into());
                return (Mode::Specialist(view), CaptainAction::None);
            }
            "m" => {
                let s = &view.specialist;
                let action = CaptainAction::PickModel(CaptainModelTarget::Crew {
                    project: view.project.clone(),
                    member: format!("{SPECIALIST_MEMBER}{}", s.id),
                    label: format!("specialist {}", clean(&s.id)),
                    current_model: s.model.clone(),
                    current_thinking: s.thinking.clone(),
                });
                return (Mode::Specialist(view), action);
            }
            "e" => {
                return (
                    Mode::Pick(Self::specialist_field_picker(view)),
                    CaptainAction::None,
                )
            }
            "D" | "shift+d" => {
                return (
                    Mode::Confirm(Confirm::DeleteSpecialist(Box::new(view))),
                    CaptainAction::None,
                )
            }
            _ => {}
        }
        if kb.matches(key, "tui.select.up") || key == "k" {
            view.scroll = view.scroll.saturating_sub(1);
        } else if kb.matches(key, "tui.select.down") || key == "j" {
            view.scroll += 1;
        } else if key == "pageup" {
            view.scroll = view.scroll.saturating_sub(page);
        } else if key == "pagedown" {
            view.scroll += page;
        }
        (Mode::Specialist(view), CaptainAction::None)
    }

    fn render_specialist(
        theme: &Theme,
        width: usize,
        view: &SpecialistView,
        rows: usize,
    ) -> Vec<Line> {
        let s = &view.specialist;
        let mut out: Vec<Line> = vec![truncate_line(
            &vec![
                Span::raw(" "),
                theme.fg_span(ThemeColor::Dim, "specialist › "),
                theme.bold(theme.fg_span(ThemeColor::Text, clean(&s.name))),
                theme.fg_span(ThemeColor::Dim, format!("  {}", clean(&s.id))),
            ],
            width,
            "…",
        )];
        let model = if s.model.is_empty() {
            "default model".to_owned()
        } else {
            clean(&s.model)
        };
        let thinking = if s.thinking.is_empty() {
            String::new()
        } else {
            format!(" · {}", clean(&s.thinking))
        };
        out.push(truncate_line(
            &vec![
                Span::raw(" "),
                theme.fg_span(ThemeColor::Muted, format!("{model}{thinking}")),
            ],
            width,
            "…",
        ));
        for line in wrap(&clean(&s.description), width.saturating_sub(2)) {
            out.push(vec![Span::raw(" "), theme.fg_span(ThemeColor::Text, line)]);
        }
        out.push(Vec::new());
        let style = crate::markdown::MarkdownStyle::from_theme(theme);
        let body: Vec<Line> =
            crate::markdown::render_markdown(&s.prompt, width.saturating_sub(2), &style)
                .into_iter()
                .map(|line| {
                    let mut row = vec![Span::raw(" ")];
                    row.extend(line);
                    row
                })
                .collect();
        let room = rows.saturating_sub(out.len()).max(1);
        let start = view.scroll.min(body.len().saturating_sub(room));
        out.extend(body.into_iter().skip(start).take(room));
        out
    }

    fn factory_key(
        &mut self,
        mut review: FactoryReview,
        key: &str,
        kb: &KeybindingsManager,
    ) -> (Mode, CaptainAction) {
        self.notice = None;
        self.error = None;
        if let Some((_, ref mut detail_scroll)) = review.node_detail {
            if kb.matches(key, "tui.select.cancel") || key == "q" || key == "left" {
                review.node_detail = None;
                return (Mode::Factory(review), CaptainAction::None);
            }
            let page = (self.viewport_rows / 2).max(1);
            if kb.matches(key, "tui.select.up") || key == "k" {
                *detail_scroll = detail_scroll.saturating_sub(1);
            } else if kb.matches(key, "tui.select.down") || key == "j" {
                *detail_scroll += 1;
            } else if key == "pageup" || kb.matches(key, "tui.select.pageUp") {
                *detail_scroll = detail_scroll.saturating_sub(page);
            } else if key == "pagedown" || kb.matches(key, "tui.select.pageDown") {
                *detail_scroll += page;
            }
            return (Mode::Factory(review), CaptainAction::None);
        }
        if kb.matches(key, "tui.select.cancel") || key == "q" || key == "left" {
            return (Mode::Browse, CaptainAction::Refresh);
        }
        let step_node = |review: &mut FactoryReview, delta: isize, viewport: usize| {
            let theme = Theme::builtin("prime", crate::theme::ColorMode::TrueColor);
            if let Some(diagram) = review.graph.as_ref().and_then(|spec| {
                crate::factory_view::machine_block_diagram(
                    &theme,
                    spec,
                    &crate::factory_view::DiagramRenderOptions::default(),
                )
            }) {
                if !diagram.node_spans.is_empty() {
                    let cur = review
                        .selected_node
                        .as_deref()
                        .and_then(|id| diagram.node_spans.iter().position(|(s, _, _)| s == id));
                    let next = match cur {
                        Some(idx) => (idx as isize + delta)
                            .clamp(0, diagram.node_spans.len() as isize - 1)
                            as usize,
                        None => 0,
                    };
                    let (ref id, top, bottom) = diagram.node_spans[next];
                    review.selected_node = Some(id.clone());
                    review.manual_scroll = false;
                    let visible = viewport.saturating_sub(10).max(4);
                    review.scroll = crate::factory_view::scroll_to_span(
                        review.scroll,
                        top,
                        bottom,
                        visible,
                        diagram.lines.len(),
                    );
                    return;
                }
            }
            if delta < 0 {
                review.scroll = review.scroll.saturating_sub(1);
            } else {
                review.scroll += 1;
            }
        };
        if kb.matches(key, "tui.select.up") || key == "k" {
            step_node(&mut review, -1, self.viewport_rows);
            return (Mode::Factory(review), CaptainAction::None);
        }
        if kb.matches(key, "tui.select.down") || key == "j" {
            step_node(&mut review, 1, self.viewport_rows);
            return (Mode::Factory(review), CaptainAction::None);
        }
        if key == "pageup" || kb.matches(key, "tui.select.pageUp") {
            review.manual_scroll = true;
            review.scroll = review
                .scroll
                .saturating_sub((self.viewport_rows / 2).max(1));
            return (Mode::Factory(review), CaptainAction::None);
        }
        if key == "pagedown" || kb.matches(key, "tui.select.pageDown") {
            review.manual_scroll = true;
            review.scroll += (self.viewport_rows / 2).max(1);
            return (Mode::Factory(review), CaptainAction::None);
        }
        if kb.matches(key, "tui.select.confirm") {
            if let Some(id) = review.selected_node.clone() {
                review.node_detail = Some((id, 0));
            }
            return (Mode::Factory(review), CaptainAction::None);
        }
        if matches!(key, "+" | "shift+=" | "x" | "b") {
            return self.factory_stage_key(review, key);
        }
        let action = match key {
            "a" if review.reviewed != review.digest => {
                self.notice = Some("Changed - press r to review again.".into());
                CaptainAction::None
            }
            "a" => match review.state.as_str() {
                "draft" | "changed" => CaptainAction::Act {
                    op: "factory-approve".into(),
                    args: json!({"project": review.project, "name": review.name, "digest": review.reviewed}),
                    done: format!("Approved this version of {}.", review.name),
                },
                "approved" => {
                    self.notice = Some("This version is approved.".into());
                    CaptainAction::None
                }
                "invalid" => {
                    self.notice = Some("Fix the errors before approval.".into());
                    CaptainAction::None
                }
                _ => {
                    self.notice = Some("This factory has no files.".into());
                    CaptainAction::None
                }
            },
            "u" => match drift_update(&review) {
                Some(_) if review.live => {
                    self.notice = Some(format!(
                        "A run of {} is live. Stop it before updating.",
                        review.name
                    ));
                    CaptainAction::None
                }
                Some((op, key, value)) => {
                    let mut args = json!({"project": review.project, "name": review.name});
                    args[key] = json!(value);
                    CaptainAction::Act {
                        op: op.into(),
                        args,
                        done: format!("Updated {value}. Review and approve the new version."),
                    }
                }
                None => {
                    self.notice = Some("Nothing to update.".into());
                    CaptainAction::None
                }
            },
            "e" if review.state != "missing" => {
                // The factory folder is <root>/.prime/machines/<name>.
                match std::path::Path::new(&review.path).ancestors().nth(3) {
                    Some(root) => CaptainAction::EditFactory {
                        root: root.to_string_lossy().into_owned(),
                        name: review.name.clone(),
                    },
                    None => CaptainAction::None,
                }
            }
            "m" if review.state != "missing" => {
                if review.live {
                    self.notice = Some(format!(
                        "A run of {} is live. Stop it before changing models.",
                        review.name
                    ));
                    CaptainAction::None
                } else if let Some(state_id) = review.selected_node.clone() {
                    let detail = review.state_details.get(&state_id);
                    let kind = detail
                        .and_then(|d| d.get("kind"))
                        .and_then(Value::as_str)
                        .or_else(|| {
                            review
                                .graph
                                .as_ref()
                                .and_then(|g| g.get("states"))
                                .and_then(Value::as_array)
                                .and_then(|states| {
                                    states.iter().find(|s| {
                                        s.get("id").and_then(Value::as_str) == Some(&state_id)
                                    })
                                })
                                .map(|s| {
                                    if s.get("python").is_some()
                                        || s.get("kind").and_then(Value::as_str) == Some("python")
                                    {
                                        "python"
                                    } else {
                                        "agent"
                                    }
                                })
                        })
                        .unwrap_or("agent");
                    if kind == "python" {
                        self.notice =
                            Some("Python state has no model. Press c to request a change.".into());
                        CaptainAction::None
                    } else {
                        let role = detail
                            .and_then(|d| d.get("role"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned);
                        let current_model = detail
                            .and_then(|d| d.get("model"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        let current_thinking = detail
                            .and_then(|d| d.get("thinking"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        let sharing = role.as_deref().map_or(0, |role| {
                            review.state_details.as_object().map_or(0, |states| {
                                states
                                    .values()
                                    .filter(|d| d["role"].as_str() == Some(role))
                                    .count()
                            })
                        });
                        if let (Some(role), true) = (role.clone(), sharing > 1) {
                            let items = vec![
                                PickItem {
                                    label: format!("Change role '{role}' ({sharing} states)"),
                                    value: json!("role"),
                                },
                                PickItem {
                                    label: format!("Override only '{state_id}'"),
                                    value: json!("state"),
                                },
                            ];
                            let picker = Picker::new(
                                &format!("{} · {state_id} model", review.name),
                                items,
                                PickPurpose::RoleOrState {
                                    review: Box::new(review),
                                    state_id,
                                    role,
                                    current_model,
                                    current_thinking,
                                },
                            );
                            return (Mode::Pick(picker), CaptainAction::None);
                        }
                        CaptainAction::PickModel(CaptainModelTarget::FactoryNode {
                            project: review.project.clone(),
                            name: review.name.clone(),
                            state_id,
                            role,
                            current_model,
                            current_thinking,
                        })
                    }
                } else {
                    self.notice = Some("Select a node in the diagram first.".into());
                    CaptainAction::None
                }
            }
            "c" => CaptainAction::Compose {
                purpose: ComposePurpose::Message {
                    project: review.project.clone(),
                    member: None,
                },
                label: format!("change request for the captain · {}", review.name),
                initial: format!("Change the project factory {}: ", review.name),
            },
            "D" | "shift+d" => {
                if review.live {
                    self.notice = Some(format!(
                        "A run of {} is live. Stop it before you delete the factory.",
                        review.name
                    ));
                    CaptainAction::None
                } else {
                    return (
                        Mode::Confirm(Confirm::DeleteFactory(Box::new(review))),
                        CaptainAction::None,
                    );
                }
            }
            "r" => CaptainAction::LoadFactory {
                project: review.project.clone(),
                name: review.name.clone(),
            },
            "M" | "shift+m" if review.live => {
                self.notice = Some(format!(
                    "A run of {} is live. Stop it before editing the factory.",
                    review.name
                ));
                CaptainAction::None
            }
            "M" | "shift+m" => {
                let (current_model, current_thinking) = review
                    .default_role
                    .clone()
                    .unwrap_or_else(|| (review.inherit_model.clone(), String::new()));
                CaptainAction::PickModel(CaptainModelTarget::FactoryDefault {
                    project: review.project.clone(),
                    name: review.name.clone(),
                    current_model,
                    current_thinking,
                })
            }
            _ => CaptainAction::None,
        };
        (Mode::Factory(review), action)
    }

    /// The chart-built stages of a factory under review, in diagram order.
    fn review_stages(review: &FactoryReview) -> Vec<String> {
        let ids: Vec<String> = review
            .graph
            .as_ref()
            .and_then(|g| g.get("states"))
            .and_then(Value::as_array)
            .map(|states| {
                states
                    .iter()
                    .filter_map(|s| s.get("id").and_then(Value::as_str).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_else(|| {
                review
                    .state_details
                    .as_object()
                    .map(|states| states.keys().cloned().collect())
                    .unwrap_or_default()
            });
        let mut stages: Vec<String> = Vec::new();
        for stage in ids.iter().filter_map(|id| compose_stage_of(id)) {
            if !stages.iter().any(|s| s == stage) {
                stages.push(stage.to_owned());
            }
        }
        stages
    }

    /// `+` adds a stage, `x` removes the selected node's stage, `b` reopens the chart builder.
    fn factory_stage_key(&mut self, review: FactoryReview, key: &str) -> (Mode, CaptainAction) {
        if review.live {
            self.notice = Some(format!(
                "A run of {} is live. Stop it before editing the factory.",
                review.name
            ));
            return (Mode::Factory(review), CaptainAction::None);
        }
        let stages = Self::review_stages(&review);
        match key {
            "x" => {
                let stage = review.selected_node.as_deref().and_then(compose_stage_of);
                match stage {
                    None => {
                        self.notice = Some("Select a stage node in the diagram first.".into());
                        (Mode::Factory(review), CaptainAction::None)
                    }
                    Some(_) if stages.len() <= 1 => {
                        self.notice = Some(
                            "A factory keeps at least one stage. Press D to delete it.".into(),
                        );
                        (Mode::Factory(review), CaptainAction::None)
                    }
                    Some(stage) => (
                        Mode::Confirm(Confirm::RemoveStage {
                            review: Box::new(review),
                            stage: stage.to_owned(),
                        }),
                        CaptainAction::None,
                    ),
                }
            }
            "b" => {
                let mut compose = ComposeView::new(
                    review.project.clone(),
                    review.name.clone(),
                    Some(&stages),
                    &Self::review_models(&review),
                );
                compose.replace = true;
                self.mode = Mode::Compose(compose);
                (Mode::Browse, CaptainAction::None)
            }
            _ => {
                let items: Vec<PickItem> = COMPOSE_STAGES
                    .iter()
                    .filter(|(id, _, _)| !stages.iter().any(|s| s == id))
                    .map(|(id, about, _)| PickItem {
                        label: format!("{id} · {about}"),
                        value: json!(id),
                    })
                    .collect();
                if items.is_empty() {
                    self.notice = Some("Every stage is in this factory.".into());
                    return (Mode::Factory(review), CaptainAction::None);
                }
                let title = format!("{} · add a stage", review.name);
                (
                    Mode::Pick(Picker::new(
                        &title,
                        items,
                        PickPurpose::AddStage(Box::new(review)),
                    )),
                    CaptainAction::None,
                )
            }
        }
    }

    /// The models a chart-built factory's agent stages run on, from its state details.
    fn review_models(review: &FactoryReview) -> Value {
        let mut models = serde_json::Map::new();
        if let Some(states) = review.state_details.as_object() {
            for (id, detail) in states {
                let Some(stage) = compose_stage_of(id) else {
                    continue;
                };
                let model = detail["model"].as_str().unwrap_or_default();
                if detail["kind"] == "python" || model.is_empty() || models.contains_key(stage) {
                    continue;
                }
                models.insert(
                    stage.to_owned(),
                    json!({"model": model, "thinking": detail["thinking"].as_str().unwrap_or_default()}),
                );
            }
        }
        Value::Object(models)
    }

    /// The chart builder: Space toggles, J/K reorders, m picks a model, Enter or s saves.
    fn compose_key(
        &mut self,
        mut compose: ComposeView,
        key: &str,
        kb: &KeybindingsManager,
    ) -> (Mode, CaptainAction) {
        self.notice = None;
        self.error = None;
        if kb.matches(key, "tui.select.cancel") || key == "ctrl+c" || key == "q" {
            if let Some(setup) = compose.setup.take() {
                let action = self.setup_step(*setup);
                return (std::mem::replace(&mut self.mode, Mode::Browse), action);
            }
            self.notice = Some("Chart closed without saving.".into());
            return (Mode::Browse, CaptainAction::None);
        }
        match key {
            "J" | "shift+j" | "shift+down" => compose.reorder(false),
            "K" | "shift+k" | "shift+up" => compose.reorder(true),
            "space" | " " => {
                if let Some(stage) = compose.stages.get_mut(compose.selected) {
                    stage.on = !stage.on;
                }
            }
            "m" => {
                let Some(stage) = compose.stages.get(compose.selected) else {
                    return (Mode::Compose(compose), CaptainAction::None);
                };
                if matches!(stage.id.as_str(), "sanitize-diff" | "validate") {
                    self.notice = Some(format!("{} runs code, not a model.", stage.id));
                    return (Mode::Compose(compose), CaptainAction::None);
                }
                let target = CaptainModelTarget::Crew {
                    project: compose.project.clone(),
                    member: format!("{COMPOSE_MEMBER}{}", stage.id),
                    label: format!("{} · {}", compose.name, stage.id),
                    current_model: stage.model.clone(),
                    current_thinking: stage.thinking.clone(),
                };
                return (Mode::Compose(compose), CaptainAction::PickModel(target));
            }
            "enter" | "s" => return self.compose_save(compose),
            _ if kb.matches(key, "tui.select.up") || key == "k" => {
                compose.selected = compose.selected.saturating_sub(1);
            }
            _ if kb.matches(key, "tui.select.down") || key == "j" => {
                compose.selected = (compose.selected + 1).min(compose.stages.len() - 1);
            }
            _ if kb.matches(key, "tui.select.confirm") => return self.compose_save(compose),
            _ => {}
        }
        (Mode::Compose(compose), CaptainAction::None)
    }

    fn compose_save(&mut self, mut compose: ComposeView) -> (Mode, CaptainAction) {
        if let Some(problem) = compose.problem() {
            self.notice = Some(problem.into());
            return (Mode::Compose(compose), CaptainAction::None);
        }
        let choice = compose.choice();
        if let Some(mut setup) = compose.setup.take() {
            setup.compose = Some(choice);
            setup.design = false;
            setup.machine = None;
            setup.factory_chosen = true;
            setup.step = setup.next_step(true);
            let action = self.setup_step(*setup);
            return (std::mem::replace(&mut self.mode, Mode::Browse), action);
        }
        self.notice = Some(format!("Saving {}…", compose.name));
        let action = CaptainAction::SaveFactory {
            op: "factory-compose".into(),
            project: compose.project.clone(),
            name: compose.name.clone(),
            args: json!({
                "project": compose.project,
                "name": compose.name,
                "stages": choice.stages,
                "models": choice.models,
                "replace": compose.replace,
            }),
        };
        (Mode::Compose(compose), action)
    }

    /// The shared model picker returned a model for the chart builder's selected stage.
    pub(crate) fn apply_compose_model(&mut self, stage: &str, model: String, thinking: String) {
        if let Mode::Compose(compose) = &mut self.mode {
            if let Some(row) = compose.stages.iter_mut().find(|s| s.id == stage) {
                row.model = model;
                row.thinking = thinking;
            }
        }
    }

    /// A chart edit failed: the builder or review page stays open with the error.
    pub(crate) fn compose_failed(&mut self, error: String) {
        self.notice = None;
        self.error = Some(error);
    }

    // Rendering.

    pub(crate) fn render(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        let width = width.max(20);
        let rows = self.viewport_rows;
        let mut out: Vec<Line> = Vec::new();
        let dim = |t: &str| theme.fg_span(ThemeColor::Dim, t.to_string());
        let muted = |t: &str| theme.fg_span(ThemeColor::Muted, t.to_string());
        // Header: title, project counts, this checkout.
        let projects = &self.snapshot.projects;
        let mut summary = vec![format!(
            "{} project{}",
            projects.len(),
            if projects.len() == 1 { "" } else { "s" }
        )];
        for state in ["needs you", "running", "paused"] {
            let count = projects
                .iter()
                .filter(|p| p.summary_state() == state)
                .count();
            if count > 0 {
                summary.push(format!("{count} {state}"));
            }
        }
        out.push(truncate_line(
            &vec![
                Span::raw(" "),
                theme.bold(theme.fg_span(ThemeColor::Accent, "captain")),
                Span::raw("  "),
                muted(&summary.join(" · ")),
            ],
            width,
            "…",
        ));
        out.push(vec![
            Span::raw(" "),
            dim(&truncate_to_width(
                &self.snapshot.current_root,
                width.saturating_sub(2),
                "…",
            )),
        ]);
        out.push(rule_row(theme, width));
        let body_rows = rows.saturating_sub(out.len() + 3);
        let body = match &self.mode {
            Mode::Pick(picker) => Self::render_picker(theme, width, picker, body_rows),
            Mode::Confirm(confirm) => Self::render_confirm(theme, width, confirm),
            Mode::Thread(thread) => Self::render_thread(theme, width, thread, body_rows),
            Mode::Factory(review) => Self::render_factory(theme, width, review, body_rows),
            Mode::Compose(compose) => Self::render_compose(theme, width, compose, body_rows),
            Mode::Specialist(view) => Self::render_specialist(theme, width, view, body_rows),
            Mode::History(history) => Self::render_history(theme, width, history, body_rows),
            Mode::Browse => self.render_browse(theme, width, body_rows),
        };
        out.extend(body.into_iter().take(body_rows));
        while out.len() < rows.saturating_sub(3) {
            out.push(Vec::new());
        }
        // Status line, rule, key hints.
        let status = self
            .error
            .as_ref()
            .or(self.snapshot_error.as_ref())
            .map(|e| theme.fg_span(ThemeColor::Error, clean(e)))
            .or_else(|| {
                self.notice
                    .as_ref()
                    .map(|n| theme.fg_span(ThemeColor::Text, clean(n)))
            })
            .or_else(|| (!self.loaded).then(|| dim("Loading…")));
        out.push(truncate_line(
            &vec![Span::raw(" "), status.unwrap_or_else(|| Span::raw(""))],
            width,
            "…",
        ));
        out.push(rule_row(theme, width));
        out.push(truncate_line(
            &vec![Span::raw(" "), dim(&self.footer(width, kb))],
            width,
            "…",
        ));
        out
    }

    fn footer(&self, width: usize, kb: &KeybindingsManager) -> String {
        let f6 = crate::keybindings::format_key_text(&kb.get_keys("app.factory.toggle").join("/"));
        match &self.mode {
            Mode::Pick(picker) if picker.multi => {
                "↑↓ select · Space mark · Enter next · Esc back".into()
            }
            Mode::Pick(_) => "↑↓ select · Enter choose · Esc back".into(),
            Mode::Confirm(_) => "y confirm · n cancel".into(),
            Mode::Thread(thread) if thread.returns_to_chat => {
                "↑↓ scroll · r reply · Esc back to chat".into()
            }
            Mode::Thread(_) => "↑↓ scroll · r reply · Esc back".into(),
            Mode::Specialist(_) => "↑↓ scroll · m model · e edit · D delete · Esc back".into(),
            Mode::History(_) => "↑↓ select · r restore · Esc back".into(),
            Mode::Compose(_) if width < 90 => {
                "↑↓ stage · Space toggle · J/K move · m model · Enter save · Esc back".into()
            }
            Mode::Compose(_) => {
                "↑↓ stage · Space toggle · J/K or Shift+↑↓ reorder · m model+thinking · Enter/s save & open diagram · Esc back".into()
            }
            Mode::Factory(review) if review.node_detail.is_some() => {
                "↑/↓ scroll · PageUp/PageDown page · Esc back".into()
            }
            Mode::Factory(review) if drift_update(review).is_some() && width < 90 => {
                "↑/↓ node · Enter files · u update · a approve · c change · Esc back".into()
            }
            Mode::Factory(review) if drift_update(review).is_some() => {
                "↑/↓ node · Enter files · m model · M default · u update · a approve · e edit · c change · D delete · Esc back".into()
            }
            Mode::Factory(_) if width < 90 => {
                "↑/↓ node · Enter files · m model · a approve · e edit · c change · Esc back".into()
            }
            Mode::Factory(_) if width < 116 => {
                "↑/↓ node · Enter files · m/M model · + stage · x remove · b chart · a approve · e edit · c change · D delete · Esc back".into()
            }
            Mode::Factory(_) => {
                "↑/↓ node · PageUp/PageDown scroll · Enter files · m model · M default model · + add stage · x remove stage · b edit chart · a approve · e edit · c change · D delete · Esc back".into()
            }
            Mode::Browse => self.browse_footer(width, &f6),
        }
    }

    fn browse_footer(&self, width: usize, f6: &str) -> String {
        match self.tab {
            Tab::Digest => {
                return format!("↑↓ scroll · e edit · h history · 1-4 tab · {f6} close");
            }
            Tab::Drive => {
                return format!(
                    "↑↓ file · PgUp/PgDn scroll · Enter open · e edit · h history · 1-4 tab · {f6} close"
                );
            }
            Tab::Factories => {
                return format!(
                    "↑↓ select · Enter review · n design · b build from chart · a approve · D delete · 1-4 tab · {f6} close"
                );
            }
            Tab::Overview => {}
        }
        match self.drawer {
            Some(Drawer::Wake(_)) => {
                return "↑↓ event · Space/Enter cycle policy · w/Esc close".into();
            }
            Some(Drawer::Schedules(_)) => {
                return "↑↓ schedule · Space pause/enable · x delete · S/Esc close".into();
            }
            None => {}
        }
        if let Some(item) = self.selected_need() {
            return format!("{} · Esc close", item.hint());
        }
        if self.focus == Focus::Crew && width >= 90 {
            return format!(
                "↑↓ select · Enter open · n/N new chat · a add thread · d assign · v review · ? help · {f6} close"
            );
        }
        if width < 90 {
            format!("↑↓ · Tab · Enter open · s stop · D delete · ? help · {f6} close")
        } else if width < 114 {
            format!(
                "↑↓ · Tab crew · 1-4 tab · Enter open · m message · s stop · R resume · D delete · ? help · {f6} close"
            )
        } else {
            format!(
                "↑↓ select · Tab crew · 1-4 tab · Enter open · m message · A archive · s stop · R resume · D delete · ? help · {f6} close"
            )
        }
    }

    fn render_browse(&self, theme: &Theme, width: usize, rows: usize) -> Vec<Line> {
        let rail = if width >= 72 {
            (width / 3).clamp(22, 36)
        } else {
            (width / 3).max(18)
        };
        let left = self.render_rail(theme, rail, rows);
        let right_width = width.saturating_sub(rail + 3);
        let right = if self.help {
            Self::render_help(theme, right_width)
        } else {
            self.render_details(theme, right_width, rows)
        };
        let border = theme.fg_span(ThemeColor::BorderMuted, "│");
        (0..rows)
            .map(|row| {
                let mut line = truncate_line(left.get(row).unwrap_or(&Vec::new()), rail, "…");
                let pad = rail.saturating_sub(line_width(&line));
                line.push(Span::raw(" ".repeat(pad)));
                line.push(border.clone());
                line.push(Span::raw(" "));
                line.extend(truncate_line(
                    right.get(row).unwrap_or(&Vec::new()),
                    right_width,
                    "…",
                ));
                line
            })
            .collect()
    }

    fn render_rail(&self, theme: &Theme, width: usize, rows: usize) -> Vec<Line> {
        let mut out: Vec<Line> = vec![vec![
            Span::raw("   "),
            theme.fg_span(ThemeColor::Dim, "projects"),
        ]];
        let projects = &self.snapshot.projects;
        if projects.is_empty() {
            for text in [
                "",
                "No projects yet",
                "n  set up a Captain",
                "p  promote this chat",
                "f  open factory",
            ] {
                out.push(vec![Span::raw("   "), theme.fg_span(ThemeColor::Dim, text)]);
            }
            return out;
        }
        let slots = (rows.saturating_sub(1) / 2).max(1);
        let top = self.project.saturating_sub(slots - 1);
        for (index, project) in projects.iter().enumerate().skip(top).take(slots) {
            let selected = index == self.project;
            let state = project.summary_state();
            let (needs, running, done) = project.counts();
            let counts = [(needs, "!"), (running, "●"), (done, "✓")]
                .iter()
                .filter(|(n, _)| *n > 0)
                .map(|(n, g)| format!("{g}{n}"))
                .collect::<Vec<_>>()
                .join(" ");
            let marker = if selected && self.focus == Focus::Projects {
                "›"
            } else if selected {
                "·"
            } else {
                " "
            };
            let name_width = width.saturating_sub(6 + str_width(&counts) + 1);
            let name = truncate_to_width(&clean(&project.name), name_width, "…");
            let mut first = vec![
                Span::raw(" "),
                theme.bold(theme.fg_span(ThemeColor::Accent, marker)),
                Span::raw(" "),
                theme.fg_span(summary_color(state), "■"),
                Span::raw(" "),
                if selected {
                    theme.bold(theme.fg_span(ThemeColor::Text, name.clone()))
                } else {
                    theme.fg_span(ThemeColor::Text, name.clone())
                },
            ];
            let used = 5 + str_width(&name);
            let gap = width.saturating_sub(used + str_width(&counts) + 1);
            first.push(Span::raw(" ".repeat(gap)));
            first.push(theme.fg_span(ThemeColor::Dim, counts));
            out.push(first);
            let here = if project.root == self.snapshot.current_root {
                "  · here"
            } else {
                ""
            };
            out.push(vec![
                Span::raw("     "),
                theme.fg_span(summary_color(state), format!("{state}{here}")),
            ]);
        }
        out
    }

    fn render_details(&self, theme: &Theme, width: usize, rows: usize) -> Vec<Line> {
        let Some(project) = self.selected_project() else {
            return vec![vec![theme.fg_span(ThemeColor::Dim, "No project selected")]];
        };
        let mut out: Vec<Line> = vec![self.tab_bar(theme, project), Vec::new()];
        let rows = rows.saturating_sub(out.len());
        out.extend(match self.tab {
            Tab::Overview => self.render_overview(theme, width, project, rows),
            Tab::Digest => self.render_digest(theme, width, project, rows),
            Tab::Drive => self.render_drive(theme, width, rows),
            Tab::Factories => self.render_factories(theme, width, project, rows),
        });
        out
    }

    /// `1 Overview  2 Digest  3 Drive  4 Factories`, the open tab in the accent.
    fn tab_bar(&self, theme: &Theme, project: &CaptainProject) -> Line {
        let mut line: Line = Vec::new();
        for (index, tab) in Tab::ALL.iter().enumerate() {
            if index > 0 {
                line.push(Span::raw("  "));
            }
            let label = format!("{} {}", index + 1, tab.label());
            line.push(if *tab == self.tab {
                theme.bold(theme.fg_span(ThemeColor::Accent, label))
            } else {
                theme.fg_span(ThemeColor::Dim, label)
            });
            let waiting = match tab {
                Tab::Overview => project.needs_items().len(),
                Tab::Factories => project
                    .factories
                    .iter()
                    .filter(|f| f.needs_review())
                    .count(),
                _ => 0,
            };
            if waiting > 0 {
                line.push(theme.fg_span(ThemeColor::Warning, format!(" !{waiting}")));
            }
        }
        line
    }

    /// The project lines every Overview starts with: name and model, root, the Captain's
    /// state, and the objective.
    fn project_header(theme: &Theme, width: usize, project: &CaptainProject) -> Vec<Line> {
        let dim = |t: String| theme.fg_span(ThemeColor::Dim, t);
        let mut title = vec![
            theme.bold(theme.fg_span(ThemeColor::Text, clean(&project.name))),
            Span::raw("  "),
            theme.fg_span(ThemeColor::MdLink, project.model_label()),
        ];
        if let Some(spend) = project.spend_label() {
            title.push(Span::raw("  "));
            title.push(if project.budget.exceeded {
                theme.bold(theme.fg_span(ThemeColor::Warning, format!("{spend} over budget")))
            } else {
                theme.fg_span(ThemeColor::Muted, spend)
            });
        }
        let mut out: Vec<Line> = vec![
            title,
            vec![dim(truncate_to_width(&project.root, width, "…"))],
        ];
        let mut lead: Line = Vec::new();
        if project.is_active() {
            let (glyph, color) = status_glyph(if project.lead_state.is_empty() {
                "ready"
            } else {
                &project.lead_state
            });
            lead.push(theme.fg_span(
                color,
                format!("captain  {glyph} {}", project.lead_state.replace('_', " ")),
            ));
        } else {
            lead.push(theme.bold(theme.fg_span(
                ThemeColor::Error,
                format!("captain  ■ {}", project.display_state()),
            )));
            if project.display_state() == "paused" {
                lead.push(dim("  R resume".into()));
            }
        }
        if let Some(edit) = project.edits.first() {
            lead.push(dim(format!(
                "   {} updated by {}",
                edit.field.replace('_', " "),
                edit.actor
            )));
        }
        out.push(lead);
        out.push(vec![dim(clean(
            project.objective.lines().next().unwrap_or(""),
        ))]);
        out
    }

    fn render_overview(
        &self,
        theme: &Theme,
        width: usize,
        project: &CaptainProject,
        rows: usize,
    ) -> Vec<Line> {
        let mut out = Self::project_header(theme, width, project);
        let unread = project.unread_total();
        if unread > 0 {
            out.push(vec![
                theme.bold(theme.fg_span(ThemeColor::Accent, format!("•{unread} unread"))),
                theme.fg_span(ThemeColor::Dim, "  i inbox"),
            ]);
        }
        out.push(Vec::new());
        if let Some(selected) = self.inbox {
            let room = rows.saturating_sub(out.len());
            out.extend(Self::inbox_body(theme, width, project, selected, room));
            return out;
        }
        if let Some(drawer) = self.drawer {
            out.extend(Self::drawer_body(theme, width, project, drawer));
            return out;
        }
        let room = rows.saturating_sub(out.len());
        let mut body = self.overview_body(theme, width, project, true);
        // A crew too long for two rows each keeps one row each, so the whole list stays in view.
        if body.len() > room {
            body = self.overview_body(theme, width, project, false);
        }
        // Keep the selected entry visible below the project lines.
        let focus = body.iter().position(|(_, s)| *s).map_or(0, |i| i + 1);
        let start = if focus >= room { focus + 1 - room } else { 0 };
        out.extend(body.into_iter().skip(start).map(|(line, _)| line));
        out
    }

    /// The Notifications Inbox: one row per notification, unread ones dotted, newest first.
    fn inbox_body(
        theme: &Theme,
        width: usize,
        project: &CaptainProject,
        selected: usize,
        room: usize,
    ) -> Vec<Line> {
        let notes = project.inbox();
        let unread = notes.iter().filter(|n| n.is_unread()).count();
        let mut out: Vec<Line> = vec![vec![
            theme.bold(theme.fg_span(ThemeColor::Text, "Inbox")),
            theme.fg_span(
                ThemeColor::Dim,
                format!("  {unread} unread · Enter open · r mark all read · i back"),
            ),
        ]];
        if notes.is_empty() {
            out.push(vec![
                theme.fg_span(ThemeColor::Dim, "No notifications yet.".to_owned())
            ]);
            return out;
        }
        let selected = selected.min(notes.len() - 1);
        let room = room.saturating_sub(1).max(1);
        let start = (selected + 1).saturating_sub(room);
        for (index, note) in notes.iter().enumerate().skip(start).take(room) {
            let is_selected = index == selected;
            let mut left = vec![if note.is_unread() {
                theme.fg_span(ThemeColor::Accent, "• ")
            } else {
                theme.fg_span(ThemeColor::Dim, "  ")
            }];
            left.push(theme.fg_span(ThemeColor::Dim, format!("{} ", clean(&note.kind))));
            left.push(theme.fg_span(ThemeColor::Text, clean(&note.title)));
            let detail = clean(&note.detail);
            if !detail.is_empty() {
                left.push(theme.fg_span(ThemeColor::Dim, format!(" · {detail}")));
            }
            let age = note.age();
            let state = if note.is_unread() { "unread" } else { "read" };
            let mut trailing: Vec<MenuSegment<'_>> = Vec::new();
            if let Some(age) = &age {
                trailing.push(MenuSegment::muted(age));
            }
            trailing.push(MenuSegment::muted(state));
            out.push(menu_row(theme, width, left, &trailing, is_selected));
        }
        out
    }

    /// The Wake Policies or Schedules drawer: a heading with its keys, then one row each.
    fn drawer_body(
        theme: &Theme,
        width: usize,
        project: &CaptainProject,
        drawer: Drawer,
    ) -> Vec<Line> {
        let heading = |title: &str, keys: &str| -> Line {
            vec![
                theme.bold(theme.fg_span(ThemeColor::Text, title.to_owned())),
                theme.fg_span(ThemeColor::Dim, format!("  {keys}")),
            ]
        };
        let mut out: Vec<Line> = Vec::new();
        match drawer {
            Drawer::Wake(selected) => {
                out.push(heading(
                    "Wake Policies",
                    "Space/Enter cycle react → context → ignore · Esc close",
                ));
                for (index, event) in WAKE_EVENTS.iter().enumerate() {
                    let policy = project
                        .wake_policy
                        .get(*event)
                        .and_then(Value::as_str)
                        .unwrap_or("react")
                        .to_owned();
                    let color = match policy.as_str() {
                        "react" => ThemeColor::Accent,
                        "ignore" => ThemeColor::Dim,
                        _ => ThemeColor::Text,
                    };
                    out.push(menu_row(
                        theme,
                        width,
                        vec![theme.fg_span(ThemeColor::Text, event.replace('_', " "))],
                        &[MenuSegment::themed(color, &policy)],
                        index == selected,
                    ));
                }
                if let Some(debounce) = project.wake_policy.get("debounce_seconds") {
                    out.push(vec![
                        theme.fg_span(ThemeColor::Dim, format!("debounce {debounce}s"))
                    ]);
                }
            }
            Drawer::Schedules(selected) => {
                out.push(heading(
                    "Schedules",
                    "Space pause/enable · x delete · Esc close",
                ));
                if project.schedules.is_empty() {
                    out.push(vec![theme.fg_span(
                        ThemeColor::Dim,
                        "No schedules. Ask the Captain to add one.",
                    )]);
                }
                for (index, schedule) in project.schedules.iter().enumerate() {
                    let next = value_text(schedule.next_run_at.as_ref());
                    let next = if next.is_empty() {
                        String::new()
                    } else {
                        format!("next {next}")
                    };
                    let (state, color) = if schedule.enabled {
                        ("enabled", ThemeColor::Success)
                    } else {
                        ("paused", ThemeColor::Warning)
                    };
                    let mut trailing = Vec::new();
                    if !next.is_empty() {
                        trailing.push(MenuSegment::muted(&next));
                    }
                    trailing.push(MenuSegment::themed(color, state));
                    out.push(menu_row(
                        theme,
                        width,
                        vec![
                            theme.fg_span(ThemeColor::MdLink, clean(&schedule.cron_expr)),
                            Span::raw("  "),
                            theme.fg_span(ThemeColor::Text, clean(&schedule.title)),
                        ],
                        &trailing,
                        index == selected,
                    ));
                }
            }
        }
        out
    }

    /// The decision card under a selected decision: summary, recommendation, option keys.
    fn decision_card(theme: &Theme, width: usize, item: &NeedsYouItem) -> Vec<Line> {
        let inner = width.saturating_sub(4);
        let mut out: Vec<Line> = Vec::new();
        let title = clean(&item.title);
        if !title.is_empty() && title != clean(&item.label) {
            out.push(vec![
                Span::raw("    "),
                theme.bold(theme.fg_span(ThemeColor::Text, title)),
            ]);
        }
        for line in wrap(&clean(&item.summary), inner).into_iter().take(3) {
            if !line.is_empty() {
                out.push(vec![
                    Span::raw("    "),
                    theme.fg_span(ThemeColor::Muted, line),
                ]);
            }
        }
        let recommendation = clean(&item.recommendation);
        if !recommendation.is_empty() {
            out.push(truncate_line(
                &vec![
                    Span::raw("    "),
                    theme.fg_span(ThemeColor::Success, "recommend "),
                    theme.fg_span(ThemeColor::Text, recommendation),
                ],
                width,
                "…",
            ));
        }
        let mut pills: Line = vec![Span::raw("    ")];
        for option in &item.options {
            pills.push(
                theme.bold(theme.fg_span(ThemeColor::Accent, format!("[{}]", clean(&option.key)))),
            );
            pills.push(theme.fg_span(ThemeColor::Text, format!(" {}  ", clean(&option.label))));
        }
        pills.push(theme.bold(theme.fg_span(ThemeColor::Accent, "[m]")));
        pills.push(theme.fg_span(ThemeColor::Dim, " custom reply"));
        out.push(truncate_line(&pills, width, "…"));
        out
    }

    /// The Overview's rows below the project lines, each with whether it is the selected
    /// row: the needs-you items, then the crew by activity, one or two rows per member.
    fn overview_body(
        &self,
        theme: &Theme,
        width: usize,
        project: &CaptainProject,
        two_rows: bool,
    ) -> Vec<(Line, bool)> {
        let dim = |t: String| theme.fg_span(ThemeColor::Dim, t);
        let selected = self.selected_entry().map(|e| e.key());
        let mut body: Vec<(Line, bool)> = Vec::new();
        let needs = project.needs_items();
        if !needs.is_empty() {
            body.push((
                vec![theme.bold(
                    theme.fg_span(ThemeColor::Warning, format!("Needs you ({})", needs.len())),
                )],
                false,
            ));
            for item in needs.iter().take(NEEDS_SHOWN) {
                let is_selected = selected.as_deref() == Some(&Entry::Need(item.clone()).key());
                body.push((Self::need_row(theme, item, is_selected, width), is_selected));
                if is_selected && item.kind == "decision" {
                    body.extend(
                        Self::decision_card(theme, width, item)
                            .into_iter()
                            .map(|line| (line, false)),
                    );
                }
            }
            if needs.len() > NEEDS_SHOWN {
                body.push((
                    vec![dim(format!("  +{} more", needs.len() - NEEDS_SHOWN))],
                    false,
                ));
            }
            body.push((Vec::new(), false));
        }
        if project.crew.is_empty() {
            body.push((
                vec![dim("No crew yet · d assign work · a add a thread".into())],
                false,
            ));
        }
        for (heading, members) in project.ordered_crew(self.show_archived) {
            if members.is_empty() {
                continue;
            }
            body.push((vec![dim(format!("{heading}  {}", members.len()))], false));
            for member in members {
                let is_selected = selected.as_deref() == Some(&Entry::Member(member.clone()).key());
                let [first, second] = Self::member_rows(
                    theme,
                    member,
                    is_selected,
                    width,
                    self.subagent_count(member),
                );
                body.push((first, is_selected));
                if two_rows {
                    body.push((second, false));
                }
                for agent in member.shown_steps() {
                    let is_selected = selected.as_deref() == Some(&step_key(&member.id, agent));
                    body.push((
                        Self::step_row(theme, agent, is_selected, width),
                        is_selected,
                    ));
                }
            }
        }
        let archived = project.archived_count();
        if archived > 0 && !self.show_archived {
            body.push((vec![dim(format!("{archived} archived · A show"))], false));
        }
        body
    }

    /// One needs-you item: its gate glyph, label, and detail; the selected row names its keys.
    fn need_row(theme: &Theme, item: &NeedsYouItem, selected: bool, width: usize) -> Line {
        let (glyph, color) = match item.kind.as_str() {
            "paused_run" => ("◐", ThemeColor::Warning),
            "pending_grant" => ("◆", ThemeColor::Warning),
            "worktree_missing" => ("✗", ThemeColor::Error),
            "blocked_crew" if item.state == "failed" => ("✗", ThemeColor::Error),
            "decision" => ("?", ThemeColor::Accent),
            "budget_exceeded" => ("$", ThemeColor::Warning),
            _ => ("!", ThemeColor::Warning),
        };
        let label = match item.kind.as_str() {
            _ if !item.label.is_empty() => clean(&item.label),
            "decision" => format!("decision · {}", clean(&item.title)),
            "budget_exceeded" => "budget exceeded".into(),
            _ => String::new(),
        };
        let mut facts: Vec<String> = Vec::new();
        match item.kind.as_str() {
            "draft_factory" => facts.push("draft · needs approval".into()),
            "changed_factory" if item.detail.is_empty() => {
                facts.push("changed · needs approval".into());
            }
            "paused_run" => facts.push("paused".into()),
            _ => {}
        }
        facts.extend(item.step.iter().cloned());
        let detail = clean(&item.detail);
        if !detail.is_empty() {
            facts.push(detail);
        }
        let mut left = vec![
            theme.fg_span(color, glyph),
            Span::raw(" "),
            theme.fg_span(ThemeColor::Text, label),
        ];
        if !facts.is_empty() {
            left.push(theme.fg_span(ThemeColor::Dim, format!("  {}", facts.join(" · "))));
        }
        let hint = item.hint();
        let trailing: Vec<MenuSegment<'_>> = if selected && !hint.is_empty() {
            vec![MenuSegment::themed(ThemeColor::Accent, hint)]
        } else {
            Vec::new()
        };
        let mut row = menu_row(theme, width, left, &trailing, selected);
        if selected {
            if let Some(marker) = row.first_mut() {
                marker.style = marker.style.patch(theme.fg_style(ThemeColor::Accent));
            }
        }
        row
    }

    fn render_digest(
        &self,
        theme: &Theme,
        width: usize,
        project: &CaptainProject,
        rows: usize,
    ) -> Vec<Line> {
        let mut out: Vec<Line> = vec![vec![
            theme.bold(theme.fg_span(ThemeColor::Text, "DIGEST.md")),
            theme.fg_span(ThemeColor::Dim, "  e edit · h history"),
        ]];
        let digest = project.digest.trim();
        let body = if digest.is_empty() {
            let report = project
                .lead_detail
                .as_deref()
                .map(clean)
                .unwrap_or_default();
            vec![vec![theme.fg_span(
                ThemeColor::Dim,
                if report.is_empty() {
                    "No digest yet. The Captain writes one after crew reports.".to_owned()
                } else {
                    format!("report {report}")
                },
            )]]
        } else {
            Self::markdown(theme, digest, width)
        };
        out.extend(Self::scrolled(
            body,
            self.digest_scroll,
            rows.saturating_sub(1),
        ));
        out
    }

    fn render_drive(&self, theme: &Theme, width: usize, rows: usize) -> Vec<Line> {
        let Some(project) = self.selected_project() else {
            return Vec::new();
        };
        let files = self.drive_files();
        let mut out: Vec<Line> = Vec::new();
        for (index, file) in files.iter().enumerate() {
            let (name, size) = match file {
                DriveFile::Field { name, field } => {
                    let text = match *field {
                        "rules" => &project.rules,
                        "crew_rules" => &project.crew_rules,
                        "plan" => &project.plan,
                        _ => &project.digest,
                    };
                    let lines = text.trim().lines().count();
                    ((*name).to_owned(), format!("{lines} lines"))
                }
                DriveFile::Specialist(s) => {
                    let detail = specialist_label(s);
                    let rest = detail
                        .split_once(" · ")
                        .map_or("", |(_, rest)| rest)
                        .to_owned();
                    (format!("specialists/{}.md", clean(&s.id)), rest)
                }
                DriveFile::Saved { dir, name } => (
                    format!("{dir}/{}", clean(name)),
                    if *dir == "reports" {
                        "report"
                    } else {
                        "handoff"
                    }
                    .to_owned(),
                ),
            };
            out.push(menu_row(
                theme,
                width,
                vec![theme.fg_span(ThemeColor::Text, name)],
                &[MenuSegment::muted(&size)],
                index == self.drive,
            ));
        }
        out.push(rule_row(theme, width));
        let body = match files.get(self.drive) {
            Some(DriveFile::Field { field, .. }) => {
                let text = match *field {
                    "rules" => &project.rules,
                    "crew_rules" => &project.crew_rules,
                    "plan" => &project.plan,
                    _ => &project.digest,
                };
                if text.trim().is_empty() {
                    vec![vec![theme.fg_span(ThemeColor::Dim, "empty · e write it")]]
                } else {
                    Self::markdown(theme, text.trim(), width)
                }
            }
            Some(DriveFile::Specialist(s)) => {
                let mut body = Vec::new();
                for line in wrap(&clean(&s.description), width.saturating_sub(2)) {
                    body.push(vec![Span::raw(" "), theme.fg_span(ThemeColor::Muted, line)]);
                }
                body.extend(Self::markdown(theme, &s.prompt, width));
                body
            }
            Some(DriveFile::Saved { dir, name }) => match saved_preview(&project.root, dir, name) {
                Some(text) if !text.trim().is_empty() => Self::markdown(theme, text.trim(), width),
                Some(_) => vec![vec![theme.fg_span(ThemeColor::Dim, "empty")]],
                None => vec![vec![theme.fg_span(
                    ThemeColor::Dim,
                    format!("Not readable here: .prime/captain/{dir}/{}", clean(name)),
                )]],
            },
            None => Vec::new(),
        };
        let room = rows.saturating_sub(out.len());
        out.extend(Self::scrolled(body, self.drive_scroll, room));
        out
    }

    fn render_factories(
        &self,
        theme: &Theme,
        width: usize,
        project: &CaptainProject,
        rows: usize,
    ) -> Vec<Line> {
        let mut out: Vec<Line> = Vec::new();
        if project.factories.is_empty() {
            out.push(vec![theme.fg_span(
                ThemeColor::Dim,
                "No project factories · n ask the Captain to design one",
            )]);
        }
        let room = rows.saturating_sub(2).max(1);
        let top = self.factory.saturating_sub(room - 1);
        for (index, factory) in project.factories.iter().enumerate().skip(top).take(room) {
            let (glyph, color) = factory_glyph(&factory.state);
            let mut trailing = vec![MenuSegment::muted(factory_state_label(
                &factory.state,
                factory.approved,
            ))];
            if !factory.drift.is_empty() {
                trailing.push(MenuSegment::themed(ThemeColor::Warning, &factory.drift));
            }
            out.push(menu_row(
                theme,
                width,
                vec![
                    theme.fg_span(color, glyph),
                    Span::raw(" "),
                    theme.fg_span(ThemeColor::Text, clean(&factory.name)),
                ],
                &trailing,
                index == self.factory,
            ));
        }
        if let Some(error) = &project.factories_error {
            out.push(vec![theme.fg_span(ThemeColor::Error, clean(error))]);
        }
        out
    }

    fn markdown(theme: &Theme, text: &str, width: usize) -> Vec<Line> {
        let style = crate::markdown::MarkdownStyle::from_theme(theme);
        crate::markdown::render_markdown(text, width.saturating_sub(2), &style)
            .into_iter()
            .map(|line| {
                let mut row = vec![Span::raw(" ")];
                row.extend(line);
                row
            })
            .collect()
    }

    /// `room` rows of `body` from `scroll`, clamped so the last page stays full.
    fn scrolled(body: Vec<Line>, scroll: usize, room: usize) -> Vec<Line> {
        let room = room.max(1);
        let start = scroll.min(body.len().saturating_sub(room));
        body.into_iter().skip(start).take(room).collect()
    }

    /// One step subagent under its run: `└─ ◐ implement · gemini-4-argon · high · running`,
    /// then its live progress note.
    fn step_row(theme: &Theme, agent: &StepAgent, selected: bool, width: usize) -> Line {
        let (glyph, color) = agent.glyph();
        let mut trailing: Vec<MenuSegment<'_>> = Vec::new();
        let note = agent.progress_note.as_deref().map(clean);
        if let Some(note) = note.as_deref().filter(|n| !n.is_empty()) {
            trailing.push(MenuSegment::muted(note));
        }
        if selected {
            trailing.push(MenuSegment::muted("Enter open"));
        }
        let mut row = menu_row(
            theme,
            width,
            vec![
                theme.fg_span(ThemeColor::Dim, "  └─ "),
                theme.fg_span(color, glyph),
                Span::raw(" "),
                theme.fg_span(ThemeColor::Text, agent.summary()),
            ],
            &trailing,
            selected,
        );
        if selected {
            if let Some(marker) = row.first_mut() {
                marker.style = marker.style.patch(theme.fg_style(ThemeColor::Accent));
            }
        }
        row
    }

    /// One crew member as two rows: the state icon and name with its kind, then the state text
    /// and the latest detail. The selected member's rows carry the soft selection band.
    fn member_rows(
        theme: &Theme,
        member: &CaptainMember,
        selected: bool,
        width: usize,
        subagents: usize,
    ) -> [Line; 2] {
        let (glyph, color) = activity_glyph(member.activity());
        let label = member.label();
        let stage = member.stage_badge();
        let kind = if member.is_run() {
            "factory".to_owned()
        } else if member.session.is_some() {
            "chat".into()
        } else {
            member.provider.clone()
        };
        let model_badge = (!member.is_run())
            .then(|| member.short_model_badge())
            .flatten();
        let diff = member.diff_label();
        let agents = (subagents > 0).then(|| format!("◆{subagents}"));
        let age = member.updated_at.map(|at| age_label(now_seconds() - at));
        let mut trailing: Vec<MenuSegment<'_>> = Vec::new();
        if member.needs_user() {
            trailing.push(MenuSegment::themed(ThemeColor::Warning, "! needs you"));
        }
        if let Some(stage) = &stage {
            trailing.push(MenuSegment::themed(ThemeColor::Accent, stage));
        }
        let stack = member.stack_badge();
        if let Some(stack) = &stack {
            trailing.push(MenuSegment::themed(ThemeColor::MdLink, stack));
        }
        let pr = member.pr_pill();
        if let Some((pill, color)) = &pr {
            trailing.push(MenuSegment::themed(*color, pill));
        }
        let findings =
            (member.open_findings_count > 0).then(|| format!("!{}", member.open_findings_count));
        if let Some(findings) = &findings {
            trailing.push(MenuSegment::themed(ThemeColor::Warning, findings));
        }
        for part in [&model_badge, &diff, &agents, &age].into_iter().flatten() {
            trailing.push(MenuSegment::muted(part));
        }
        trailing.push(MenuSegment::muted(&kind));
        let mut left = vec![theme.fg_span(color, glyph), Span::raw(" ")];
        if member.unread {
            left.push(theme.fg_span(ThemeColor::Accent, "• "));
        }
        left.push(theme.fg_span(ThemeColor::Text, label.clone()));
        let mut first = menu_row(theme, width, left, &trailing, selected);
        if selected {
            // The selection marker uses the accent, as in the project rail.
            if let Some(marker) = first.first_mut() {
                marker.style = marker.style.patch(theme.fg_style(ThemeColor::Accent));
            }
        }
        let mut facts: Vec<String> = Vec::new();
        if member.is_run() {
            facts.extend(
                [&member.progress, &member.step]
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
            if let Some((_, task)) = member.title.split_once(" · ") {
                let task = clean(task);
                if !label.contains(&task) {
                    facts.push(task);
                }
            }
        }
        let detail = clean(&member.detail);
        if !detail.is_empty() {
            facts.push(detail);
        }
        let mut second = vec![Span::raw("    "), theme.fg_span(color, member.state_text())];
        if !facts.is_empty() {
            second.push(theme.fg_span(ThemeColor::Dim, format!(" · {}", facts.join(" · "))));
        }
        let second = crate::menu_panel::fill_row(
            &truncate_line(&second, width, "…"),
            selected,
            width,
            theme.soft_selection_style(),
        );
        [first, second]
    }

    fn render_help(theme: &Theme, width: usize) -> Vec<Line> {
        let rows = [
            "1-4 or [ ] tabs: Overview · Digest · Drive · Factories",
            "Tab focus crew · ↑↓ move · ←→ project · → subagents of a crew row",
            "Enter open · m message · M model · v review · A archive",
            "Needs you: a approve · g grant · x decline · r retry or recreate · s stop",
            "n new Captain · p promote this chat · e edit",
            "Crew: n/N new chat · a add thread · d assign · v review",
            "a add a thread · d assign work · u release",
            "f factory · c consensus",
            "Runs: Enter on a └─ step enters its subagent · l/f live factory page",
            "i inbox: Enter open · r mark all read · i back",
            "F project factories: review, approve, design",
            "S schedules (specialists when none): Space pause · x delete",
            "w wake policies · Decision: 1-6 pick · m reply · Budget: b +$5 · c clear",
            "Drive: e edit · h history · r restore · reports/ and handoffs/ preview",
            "s stop · R resume · D delete",
            "In a Captain chat: F7 switch chat · F8/Shift+F8 next/previous · Shift+F7 captain",
            "r refresh · ? close help · Esc close",
        ];
        let mut out = vec![
            Vec::new(),
            vec![theme.bold(theme.fg_span(ThemeColor::Text, "captain controls"))],
        ];
        out.extend(
            rows.iter().map(|row| {
                vec![theme.fg_span(ThemeColor::Muted, truncate_to_width(row, width, "…"))]
            }),
        );
        out
    }

    fn render_picker(theme: &Theme, width: usize, picker: &Picker, rows: usize) -> Vec<Line> {
        let mut out: Vec<Line> = Vec::new();
        if let PickPurpose::Setup(setup) = &picker.purpose {
            let mut steps: Line = vec![Span::raw(" ")];
            for (index, name) in SETUP_STEPS.iter().enumerate() {
                if index == FACTORY_MODELS_STEP && !setup.design {
                    continue;
                }
                let color = if index == setup.step {
                    ThemeColor::Accent
                } else {
                    ThemeColor::Dim
                };
                steps.push(theme.fg_span(color, *name));
                steps.push(Span::raw("   "));
            }
            out.push(truncate_line(&steps, width, ""));
        }
        out.push(vec![
            Span::raw(" "),
            theme.bold(theme.fg_span(ThemeColor::Text, picker.title.clone())),
        ]);
        out.extend(crate::menu_panel::search_field_lines(
            theme,
            width,
            &picker.filter,
            picker.filter.chars().count(),
            true,
            "Type to filter",
        ));
        let visible = picker.visible();
        if picker.items.is_empty() {
            out.push(vec![
                Span::raw("   "),
                theme.fg_span(ThemeColor::Dim, "Loading…"),
            ]);
        } else if visible.is_empty() {
            out.push(crate::menu_panel::no_match_row(theme, width, "No match"));
        }
        let room = rows.saturating_sub(out.len()).max(1);
        let top = picker.selected.saturating_sub(room - 1);
        for (offset, &index) in visible.iter().enumerate().skip(top).take(room) {
            let selected = offset == picker.selected;
            let mark = if picker.multi {
                if picker.marked.contains(&index) {
                    "■ "
                } else {
                    "□ "
                }
            } else {
                ""
            };
            let label = format!("{mark}{}", picker.items[index].label);
            out.push(menu_row(
                theme,
                width,
                vec![theme.fg_span(ThemeColor::Text, label)],
                &[],
                selected,
            ));
        }
        out
    }

    fn render_confirm(theme: &Theme, width: usize, confirm: &Confirm) -> Vec<Line> {
        let (title, body): (String, Vec<String>) = match confirm {
            Confirm::Stop { name, .. } => (
                format!("stop the Captain of {name}?"),
                vec![
                    "Queued work is cancelled.".into(),
                    "Running crew turns and Captain-started factory runs stop.".into(),
                    "No new turns start, including digest wakeups, until you resume.".into(),
                    "Threads, transcripts, and worktrees stay.".into(),
                ],
            ),
            Confirm::Delete {
                name,
                crew,
                running,
                ..
            } => (
                format!("delete the Captain of {name}?"),
                vec![
                    if *running > 0 {
                        format!("{running} running turn(s) or run(s) stop first.")
                    } else {
                        "Nothing is running.".into()
                    },
                    "The Captain's project record, plan, rules, digest, and reports are removed."
                        .into(),
                    format!(
                        "The Captain thread and {crew} crew thread(s) return to the thread list."
                    ),
                    "Transcripts and worktrees stay.".into(),
                ],
            ),
            Confirm::DeleteFactory(review) => (
                format!("delete the project factory {}?", review.name),
                vec![
                    format!("Its folder {} is removed.", review.path),
                    "Its approval is removed. Runs that ended keep their records.".into(),
                ],
            ),
            Confirm::DeleteListedFactory { name, path, .. } => (
                format!("delete the project factory {name}?"),
                vec![
                    format!("Its folder {path} is removed."),
                    "Its approval is removed. Runs that ended keep their records.".into(),
                ],
            ),
            Confirm::DeleteSpecialist(view) => (
                format!("delete the specialist {}?", clean(&view.specialist.id)),
                vec![
                    format!("Its file {} is removed.", view.specialist.path),
                    "Factories that use it need its prompt written again.".into(),
                ],
            ),
            Confirm::RemoveStage { review, stage } => (
                format!("remove the {stage} stage from {}?", review.name),
                vec![
                    "The chart is rewired without it and the factory is a draft again.".into(),
                    "Approve the new version before it runs.".into(),
                ],
            ),
            Confirm::Release { title, .. } => (
                format!("release {title} from the crew?"),
                vec!["The thread returns to the thread list. Its transcript stays.".into()],
            ),
            Confirm::Create(setup) => (
                if setup.reference.is_some() {
                    "promote this chat to Captain?".into()
                } else {
                    "create this Captain?".into()
                },
                vec![
                    format!("brief: {}", clean(&setup.brief)),
                    format!(
                        "model: {} · {}{}",
                        setup.provider,
                        if setup.model.is_empty() {
                            "default model"
                        } else {
                            &setup.model
                        },
                        if setup.thinking.is_empty() {
                            String::new()
                        } else {
                            format!(" · {}", setup.thinking)
                        }
                    ),
                    format!("factory: {}", setup.factory_summary()),
                    if setup.design {
                        format!("factory models: {}", setup.factory_models_summary())
                    } else {
                        String::new()
                    },
                    format!("workers: {}", setup.parallel),
                    format!("threads: {}", setup.crew.len()),
                ],
            ),
        };
        let mut out = vec![
            Vec::new(),
            vec![
                Span::raw(" "),
                theme.bold(theme.fg_span(
                    if matches!(
                        confirm,
                        Confirm::Delete { .. }
                            | Confirm::DeleteFactory(_)
                            | Confirm::DeleteListedFactory { .. }
                            | Confirm::DeleteSpecialist(_)
                    ) {
                        ThemeColor::Error
                    } else {
                        ThemeColor::Text
                    },
                    title,
                )),
            ],
            Vec::new(),
        ];
        for line in body.into_iter().filter(|line| !line.is_empty()) {
            for row in wrap(&line, width.saturating_sub(3)) {
                out.push(vec![
                    Span::raw("   "),
                    theme.fg_span(ThemeColor::Muted, row),
                ]);
            }
        }
        out.push(Vec::new());
        out.push(vec![
            Span::raw(" "),
            theme.fg_span(ThemeColor::Text, "y confirm · n cancel"),
        ]);
        out
    }

    fn render_compose(
        theme: &Theme,
        width: usize,
        compose: &ComposeView,
        rows: usize,
    ) -> Vec<Line> {
        let side = width >= 100;
        let left = if side { 38 } else { width };
        let mut checklist: Vec<Line> = vec![vec![
            Span::raw(" "),
            theme.bold(theme.fg_span(
                ThemeColor::Text,
                if compose.setup.is_some() {
                    "new captain · build custom FSM".to_owned()
                } else {
                    format!("build custom FSM · {}", compose.name)
                },
            )),
        ]];
        checklist.push(vec![
            Span::raw(" "),
            theme.fg_span(ThemeColor::Dim, "one end-to-end pipeline, top to bottom"),
        ]);
        checklist.push(Vec::new());
        for (index, stage) in compose.stages.iter().enumerate() {
            let about = COMPOSE_STAGES
                .iter()
                .find(|(id, _, _)| *id == stage.id)
                .map_or("", |(_, about, _)| about);
            let mark = if stage.on { "[x]" } else { "[ ]" };
            let mut spans = vec![theme.fg_span(
                if stage.on {
                    ThemeColor::Text
                } else {
                    ThemeColor::Dim
                },
                format!("{mark} {}", stage.id),
            )];
            if !stage.model.is_empty() {
                let short = stage.model.rsplit('/').next().unwrap_or(&stage.model);
                let model = if stage.thinking.is_empty() {
                    short.to_owned()
                } else {
                    format!("{short} · {}", stage.thinking)
                };
                spans.push(theme.fg_span(ThemeColor::Accent, format!("  {model}")));
            }
            checklist.push(menu_row(theme, left, spans, &[], index == compose.selected));
            if index == compose.selected {
                checklist.push(vec![
                    Span::raw("     "),
                    theme.fg_span(ThemeColor::Muted, about),
                ]);
            }
        }
        if let Some(problem) = compose.problem() {
            checklist.push(Vec::new());
            checklist.push(vec![
                Span::raw(" "),
                theme.fg_span(ThemeColor::Warning, problem),
            ]);
        }
        let chart_width = if side {
            width.saturating_sub(left + 3)
        } else {
            width
        };
        let mut chart: Vec<Line> = vec![vec![
            Span::raw(" "),
            theme.bold(theme.fg_span(ThemeColor::Text, "live chart")),
            theme.fg_span(
                ThemeColor::Dim,
                format!("  {}", compose.choice().stages.join(" → ")),
            ),
        ]];
        let selected = compose
            .stages
            .get(compose.selected)
            .filter(|s| s.on)
            .map(|s| compose_entry(&s.id));
        match crate::factory_view::machine_block_diagram(
            theme,
            &compose.spec(),
            &crate::factory_view::DiagramRenderOptions {
                selected_state: selected.as_deref(),
                live_nodes: None,
                max_width: Some(chart_width.saturating_sub(2)),
                inherit_model: Some(""),
            },
        ) {
            Some(diagram) if !diagram.lines.is_empty() => {
                for line in diagram.lines {
                    let mut row = vec![Span::raw(" ")];
                    row.extend(line);
                    chart.push(row);
                }
            }
            _ => chart.push(vec![
                Span::raw("   "),
                theme.fg_span(ThemeColor::Dim, "Turn on a stage to draw the chart."),
            ]),
        }
        if !side {
            checklist.push(rule_row(theme, width));
            checklist.extend(chart);
            return checklist
                .into_iter()
                .take(rows)
                .map(|line| truncate_line(&line, width, "…"))
                .collect();
        }
        let border = theme.fg_span(ThemeColor::BorderMuted, "│");
        (0..rows.min(checklist.len().max(chart.len())))
            .map(|row| {
                let mut line = truncate_line(checklist.get(row).unwrap_or(&Vec::new()), left, "…");
                let pad = left.saturating_sub(line_width(&line));
                line.push(Span::raw(" ".repeat(pad)));
                line.push(border.clone());
                line.push(Span::raw(" "));
                line.extend(truncate_line(
                    chart.get(row).unwrap_or(&Vec::new()),
                    chart_width,
                    "…",
                ));
                line
            })
            .collect()
    }

    fn render_factory(
        theme: &Theme,
        width: usize,
        review: &FactoryReview,
        rows: usize,
    ) -> Vec<Line> {
        if let Some((state_id, detail_scroll)) = &review.node_detail {
            let mut detail = review
                .state_details
                .get(state_id)
                .cloned()
                .unwrap_or_else(|| json!({"id": state_id}));
            detail["inherit_model"] = json!(review.inherit_model);
            let body =
                crate::factory_view::render_node_detail_lines(theme, width, state_id, &detail);
            let room = rows.max(1);
            let start = (*detail_scroll).min(body.len().saturating_sub(room));
            return body.into_iter().skip(start).take(room).collect();
        }
        let mut out = crate::factory_view::machine_header_lines(
            theme,
            width,
            &review.name,
            &review.description,
            &review.params,
        );
        let (glyph, color) = factory_glyph(&review.state);
        let digest = review.digest.get(..12).unwrap_or(&review.digest);
        let mut status_line: Line = vec![
            Span::raw(" "),
            theme.fg_span(
                color,
                format!(
                    "{glyph} {}",
                    factory_state_label(&review.state, review.approved)
                ),
            ),
        ];
        if !digest.is_empty() {
            status_line.push(theme.fg_span(ThemeColor::Dim, format!(" · digest {digest}")));
        }
        if review.live {
            status_line.push(theme.fg_span(ThemeColor::Accent, " · ● run live"));
        }
        if review.reviewed != review.digest {
            status_line.push(
                theme.bold(
                    theme.fg_span(ThemeColor::Warning, " · Changed - press r to review again"),
                ),
            );
        }
        status_line.push(theme.fg_span(
            ThemeColor::Muted,
            format!(" · default model: {}", factory_default_label(review)),
        ));
        out.push(truncate_line(&status_line, width, "…"));
        if matches!(review.state.as_str(), "draft" | "changed") {
            out.push(truncate_line(
                &vec![
                    Span::raw(" "),
                    theme.fg_span(
                        ThemeColor::Muted,
                        "Approval covers every file of this version. Python nodes run code.",
                    ),
                ],
                width,
                "…",
            ));
        }
        out.extend(vendored_rows(
            theme,
            width,
            &review.modules,
            &review.specialists,
        ));
        let mut body: Vec<Line> = Vec::new();
        for error in &review.errors {
            for row in wrap(&clean(error), width.saturating_sub(3)) {
                body.push(vec![
                    Span::raw("   "),
                    theme.fg_span(ThemeColor::Error, row),
                ]);
            }
        }
        if let Some(error) = &review.graph_error {
            for row in wrap(
                &format!("No diagram: {}", clean(error)),
                width.saturating_sub(3),
            ) {
                body.push(vec![Span::raw("   "), theme.fg_span(ThemeColor::Dim, row)]);
            }
        }
        let mut selected_span: Option<(usize, usize)> = None;
        if let Some(diagram) = review.graph.as_ref().and_then(|spec| {
            crate::factory_view::machine_block_diagram(
                theme,
                spec,
                &crate::factory_view::DiagramRenderOptions {
                    selected_state: review.selected_node.as_deref(),
                    live_nodes: None,
                    max_width: Some(width.saturating_sub(2)),
                    inherit_model: Some(&review.inherit_model),
                },
            )
        }) {
            let offset = body.len();
            if let Some(sel) = review.selected_node.as_deref() {
                if let Some((_, top, bottom)) =
                    diagram.node_spans.iter().find(|(id, _, _)| id == sel)
                {
                    selected_span = Some((offset + *top, offset + *bottom));
                }
            }
            for line in diagram.lines {
                let mut row = vec![Span::raw(" ")];
                row.extend(line);
                body.push(truncate_line(&row, width, "…"));
            }
        }
        out.push(Vec::new());
        let room = rows.saturating_sub(out.len()).max(1);
        let mut start = review.scroll.min(body.len().saturating_sub(room));
        if !review.manual_scroll {
            if let Some((top, bottom)) = selected_span {
                start = crate::factory_view::scroll_to_span(start, top, bottom, room, body.len());
            }
        }
        out.extend(body.into_iter().skip(start).take(room));
        out
    }

    fn render_thread(theme: &Theme, width: usize, thread: &ThreadView, rows: usize) -> Vec<Line> {
        let mut head: Line = vec![
            Span::raw(" "),
            theme.fg_span(ThemeColor::Dim, "captain › "),
            theme.bold(theme.fg_span(ThemeColor::Text, thread.title.clone())),
            theme.fg_span(ThemeColor::Dim, format!("  {}", thread.provider)),
        ];
        if thread.busy {
            head.push(theme.fg_span(ThemeColor::Accent, "  ● working"));
        }
        let mut body: Vec<Line> = Vec::new();
        let inner = width.saturating_sub(4);
        for (role, text) in &thread.messages {
            body.push(Vec::new());
            if role == "user" {
                for line in wrap(text, inner) {
                    body.push(vec![
                        Span::raw("  "),
                        theme.fg_span(ThemeColor::UserMessageText, line),
                    ]);
                }
            } else {
                let style = crate::markdown::MarkdownStyle::from_theme(theme);
                for line in crate::markdown::render_markdown(text, inner, &style) {
                    let mut row = vec![Span::raw("  ")];
                    row.extend(line);
                    body.push(row);
                }
            }
        }
        if thread.messages.is_empty() {
            body.push(vec![
                Span::raw("  "),
                theme.fg_span(ThemeColor::Dim, "No messages yet."),
            ]);
        }
        let room = rows.saturating_sub(1).max(1);
        let end = body
            .len()
            .saturating_sub(thread.scroll.min(body.len().saturating_sub(room)));
        let start = end.saturating_sub(room);
        let mut out = vec![head];
        out.extend(body[start..end].iter().cloned());
        out
    }
}

/// A blank Prime crew chat in `project`'s checkout; pa opens it once the backend answers.
pub(crate) fn new_thread_action(project: String) -> CaptainAction {
    CaptainAction::Act {
        op: "new-thread".into(),
        args: json!({ "project": project }),
        done: "New crew chat.".into(),
    }
}

/// The chat switcher of a Captain project (F7, `/crew`): the Captain first, then its crew,
/// each live run followed by its step subagents.
pub(crate) struct CrewPicker {
    project: String,
    current: String,
    chats: Vec<crate::session_ui::CrewChat>,
    /// Unread marks, stage badges, and step subagents by crew member id.
    extras: std::collections::HashMap<String, CrewExtras>,
    /// The selected row of [`CrewPicker::rows`].
    selected: usize,
}

/// What a crew row of the switcher shows beyond its chat.
#[derive(Debug, Clone, Default, PartialEq)]
struct CrewExtras {
    unread: bool,
    stage: Option<String>,
    steps: Vec<StepAgent>,
}

/// One row of the switcher: a chat, or a step subagent of the run chat before it.
#[derive(Debug, Clone, PartialEq)]
enum CrewRow {
    Chat(usize),
    Step(usize, StepAgent),
}

/// The crew picker's answer to a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CrewPick {
    None,
    Close,
    Open(crate::session_ui::CrewChat),
    /// `n`: open a blank crew chat.
    NewThread,
    /// `a`: add an existing thread to the crew.
    AddExisting,
}

impl CrewPicker {
    pub(crate) fn new(
        project: String,
        current: String,
        chats: Vec<crate::session_ui::CrewChat>,
    ) -> Self {
        let selected = chats.iter().position(|c| c.label == current).unwrap_or(0);
        Self {
            project,
            current,
            chats,
            extras: std::collections::HashMap::new(),
            selected,
        }
    }

    pub(crate) fn set_chats(&mut self, chats: Vec<crate::session_ui::CrewChat>) {
        let key = self.selected_key();
        self.chats = chats;
        self.reselect(key);
    }

    /// Take each crew row's unread mark, stage badge, and step subagents from the project's
    /// status, so a run lists the subagents Enter enters.
    // The session UI's picker refresh calls this once it hands the picker the project.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn set_project(&mut self, project: &CaptainProject) {
        let key = self.selected_key();
        self.extras = project
            .crew
            .iter()
            .map(|m| {
                let extras = CrewExtras {
                    unread: m.unread,
                    stage: m.stage_badge(),
                    steps: m.shown_steps().into_iter().cloned().collect(),
                };
                (m.id.clone(), extras)
            })
            .collect();
        self.reselect(key);
    }

    fn rows(&self) -> Vec<CrewRow> {
        let mut rows = Vec::new();
        for (index, chat) in self.chats.iter().enumerate() {
            rows.push(CrewRow::Chat(index));
            if chat.kind != "factory" {
                continue;
            }
            if let Some(extras) = chat.member.as_ref().and_then(|m| self.extras.get(m)) {
                rows.extend(
                    extras
                        .steps
                        .iter()
                        .map(|agent| CrewRow::Step(index, agent.clone())),
                );
            }
        }
        rows
    }

    /// The selected row's identity across a refresh: its chat label and step child.
    fn selected_key(&self) -> Option<(String, Option<String>)> {
        match self.rows().into_iter().nth(self.selected)? {
            CrewRow::Chat(index) => Some((self.chats.get(index)?.label.clone(), None)),
            CrewRow::Step(index, agent) => {
                Some((self.chats.get(index)?.label.clone(), Some(agent.child_id)))
            }
        }
    }

    fn reselect(&mut self, key: Option<(String, Option<String>)>) {
        let rows = self.rows();
        let found = key.and_then(|(label, child)| {
            rows.iter().position(|row| match row {
                CrewRow::Chat(index) => {
                    child.is_none() && self.chats.get(*index).is_some_and(|c| c.label == label)
                }
                CrewRow::Step(index, agent) => {
                    child.as_deref() == Some(agent.child_id.as_str())
                        && self.chats.get(*index).is_some_and(|c| c.label == label)
                }
            })
        });
        self.selected = found.unwrap_or(0).min(rows.len().saturating_sub(1));
    }

    /// The chat a step row opens: `act open` with `step:<session or child id>`.
    fn step_chat(
        parent: &crate::session_ui::CrewChat,
        agent: &StepAgent,
    ) -> crate::session_ui::CrewChat {
        let target = agent.target().to_owned();
        crate::session_ui::CrewChat {
            label: format!("{} › {}", parent.label, clean(&agent.state)),
            session: Some(target.clone()),
            member: Some(format!("step:{target}")),
            busy: agent.status == "running",
            status: match agent.status.as_str() {
                "running" => "working",
                "queued" | "waiting" | "paused" => "paused",
                "error" | "failed" => "failed",
                _ => "idle",
            }
            .into(),
            model: None,
            kind: "step".into(),
            run_id: None,
        }
    }

    pub(crate) fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> CrewPick {
        if kb.matches(key, "tui.select.cancel")
            || kb.matches(key, "app.captain.crew")
            || key == "ctrl+c"
        {
            return CrewPick::Close;
        }
        match key {
            "n" | "N" | "shift+n" => return CrewPick::NewThread,
            "a" => return CrewPick::AddExisting,
            _ => {}
        }
        let rows = self.rows();
        if kb.matches(key, "tui.select.up") {
            self.selected = self.selected.saturating_sub(1);
        } else if kb.matches(key, "tui.select.down") {
            self.selected = (self.selected + 1).min(rows.len().saturating_sub(1));
        } else if kb.matches(key, "tui.select.confirm") {
            match rows.get(self.selected) {
                Some(CrewRow::Chat(index)) => {
                    let chat = &self.chats[*index];
                    if chat.label == self.current {
                        return CrewPick::Close;
                    }
                    return CrewPick::Open(chat.clone());
                }
                Some(CrewRow::Step(index, agent)) => {
                    return CrewPick::Open(Self::step_chat(&self.chats[*index], agent));
                }
                None => {}
            }
        }
        CrewPick::None
    }

    pub(crate) fn render(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        let mut out: Vec<Line> = vec![
            rule_row(theme, width),
            vec![
                Span::raw(" "),
                theme.bold(theme.fg_span(ThemeColor::Text, "switch chat")),
                theme.fg_span(ThemeColor::Dim, format!("  {}", clean(&self.project))),
            ],
        ];
        for (row_index, row) in self.rows().into_iter().enumerate() {
            let selected = row_index == self.selected;
            let index = match row {
                CrewRow::Chat(index) => index,
                CrewRow::Step(_, agent) => {
                    let (glyph, color) = agent.glyph();
                    let mut trailing = Vec::new();
                    let note = agent.progress_note.as_deref().map(clean);
                    if let Some(note) = note.as_deref().filter(|n| !n.is_empty()) {
                        trailing.push(MenuSegment::muted(note));
                    }
                    trailing.push(MenuSegment::muted("subagent"));
                    out.push(menu_row(
                        theme,
                        width,
                        vec![
                            theme.fg_span(ThemeColor::Dim, "  └─ "),
                            theme.fg_span(color, glyph),
                            Span::raw(" "),
                            theme.fg_span(ThemeColor::Text, agent.summary()),
                        ],
                        &trailing,
                        selected,
                    ));
                    continue;
                }
            };
            let chat = &self.chats[index];
            let extras = chat.member.as_ref().and_then(|m| self.extras.get(m));
            let status = match chat.status.as_str() {
                "" if chat.busy => "working",
                "" => "idle",
                other => other,
            };
            let (glyph, color) = activity_glyph(status);
            let mut trailing = Vec::new();
            if chat.label == self.current {
                trailing.push(MenuSegment::muted("open"));
            }
            if let Some(stage) = extras.and_then(|e| e.stage.as_deref()) {
                trailing.push(MenuSegment::themed(ThemeColor::Accent, stage));
            }
            if let Some(model) = chat.model.as_deref() {
                trailing.push(MenuSegment::muted(model));
            }
            if chat.kind == "factory" {
                trailing.push(MenuSegment::muted("factory run"));
            } else if chat.session.is_none() && chat.member.is_some() {
                trailing.push(MenuSegment::muted(if chat.kind.is_empty() {
                    "provider thread"
                } else {
                    &chat.kind
                }));
            }
            trailing.push(match status {
                "needs_you" => MenuSegment::themed(ThemeColor::Warning, "needs you"),
                "working" => MenuSegment::themed(ThemeColor::Accent, "working"),
                "paused" => MenuSegment::themed(ThemeColor::Warning, "paused"),
                "failed" => MenuSegment::themed(ThemeColor::Error, "failed"),
                _ => MenuSegment::muted("idle"),
            });
            let mut left = vec![theme.fg_span(color, glyph), Span::raw(" ")];
            if extras.is_some_and(|e| e.unread) {
                left.push(theme.fg_span(ThemeColor::Accent, "• "));
            }
            left.push(theme.fg_span(ThemeColor::Text, clean(&chat.label)));
            out.push(menu_row(theme, width, left, &trailing, selected));
        }
        let cancel =
            crate::keybindings::format_key_text(&kb.get_keys("tui.select.cancel").join("/"));
        out.push(crate::menu_panel::hint_row(
            theme,
            width,
            &format!("↑↓ select · Enter open · n new thread · a add existing · {cancel} close"),
        ));
        out
    }
}
