//! The `/router` full-page TUI: Ramp Router V2-inspired control plane for
//! `radiata`'s local quota-, cost-, and complexity-aware model router.
//!
//! Reads and writes `~/.prime/harness/router-profile.json` (in Ramp's V2
//! profile schema), previews live tier recommendations against the current
//! quota cache (`router-quota-cache.json`) and circuit breaker state
//! (`router-health.json`), and surfaces external `@ramp/router` gateway
//! configuration when detected.

use crate::keybindings::KeybindingsManager;
use crate::menu_panel::hint_row;
use crate::theme::{Theme, ThemeBg, ThemeColor};
use crate::usage_view::{bar_cells_width, bar_color, parse_usage, ProviderUsage, UsageReport};
use crate::width::{str_width, truncate_line, truncate_to_width};
use crate::{Line, Span};
use ratatui::style::Modifier;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const PROFILE_FILENAME: &str = "router-profile.json";
const QUOTA_CACHE_FILENAME: &str = "router-quota-cache.json";
const HEALTH_FILENAME: &str = "router-health.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetSpec {
    pub slug: &'static str,
    pub label: &'static str,
    pub cost_efficient: bool,
    pub priority: &'static str,
    pub switchyard: bool,
    pub efficient_model: &'static str,
    pub capable_model: &'static str,
    pub sensitivity: &'static str,
}

pub const PRESETS: &[PresetSpec] = &[
    PresetSpec {
        slug: "coding-agents",
        label: "Coding agents",
        cost_efficient: true,
        priority: "quota_aware",
        switchyard: false,
        efficient_model: "antigravity/gemini-3.8-flash",
        capable_model: "antigravity/gemini-4-neon",
        sensitivity: "balanced",
    },
    PresetSpec {
        slug: "coding-agents-experimental",
        label: "Coding agents (Switchyard)",
        cost_efficient: true,
        priority: "quota_aware",
        switchyard: true,
        efficient_model: "antigravity/gemini-3.8-flash",
        capable_model: "antigravity/gemini-4-neon",
        sensitivity: "balanced",
    },
    PresetSpec {
        slug: "production-agents",
        label: "General purpose agents",
        cost_efficient: true,
        priority: "favor_cost",
        switchyard: false,
        efficient_model: "antigravity/gemini-3.8-flash",
        capable_model: "antigravity/gemini-4-neon",
        sensitivity: "balanced",
    },
    PresetSpec {
        slug: "low-latency",
        label: "Low latency",
        cost_efficient: false,
        priority: "throughput_only",
        switchyard: false,
        efficient_model: "antigravity/gemini-3.8-flash",
        capable_model: "antigravity/gemini-4-neon",
        sensitivity: "balanced",
    },
    PresetSpec {
        slug: "automations",
        label: "Automations",
        cost_efficient: true,
        priority: "cost_only",
        switchyard: false,
        efficient_model: "openai-codex/gpt-5.4-mini",
        capable_model: "antigravity/gemini-4-neon",
        sensitivity: "frugal",
    },
    PresetSpec {
        slug: "knowledge-work",
        label: "Research & analysis",
        cost_efficient: true,
        priority: "favor_cost",
        switchyard: false,
        efficient_model: "antigravity/gemini-3.8-flash",
        capable_model: "anthropic/claude-opus-5-5",
        sensitivity: "quality",
    },
];

pub const MODES: &[(&str, &str)] = &[
    ("auto", "Auto (reroute on 98%+ quota, errors & retries)"),
    ("on", "Active (steer proactively on 90%+ quota & Switchyard)"),
    ("off", "Off (always keep pinned models)"),
];

pub const PRIORITIES: &[(&str, &str)] = &[
    ("quota_aware", "Quota-aware"),
    ("favor_cost", "Prefer cheaper"),
    ("cost_only", "Cheapest"),
    ("favor_throughput", "Prefer faster"),
    ("throughput_only", "Fastest"),
];

pub const SENSITIVITIES: &[(&str, &str)] = &[
    ("frugal", "Frugal (frontier only -> capable)"),
    ("balanced", "Balanced (strong/frontier -> capable)"),
    ("quality", "Quality (balanced+ -> capable)"),
];

#[derive(Debug, Clone, Copy)]
struct CatalogEntry {
    model: &'static str,
    tier: &'static str,
    pool: &'static str,
    cost: f64,
    thinking: &'static str,
}

const CATALOG: &[CatalogEntry] = &[
    CatalogEntry { model: "anthropic/claude-opus-5-5", tier: "frontier", pool: "claude", cost: 25.0, thinking: "xhigh" },
    CatalogEntry { model: "openai-codex/gpt-5.5", tier: "frontier", pool: "codex", cost: 15.0, thinking: "xhigh" },
    CatalogEntry { model: "openai-codex/gpt-6.1-sol", tier: "frontier", pool: "codex", cost: 16.0, thinking: "xhigh" },
    CatalogEntry { model: "antigravity/gemini-4-neon", tier: "frontier", pool: "antigravity-gemini", cost: 15.0, thinking: "xhigh" },
    CatalogEntry { model: "antigravity/claude-opus-5-5", tier: "frontier", pool: "antigravity-claude-gpt", cost: 25.0, thinking: "high" },
    CatalogEntry { model: "cursor/claude-opus-5-5", tier: "frontier", pool: "cursor-third-party", cost: 25.0, thinking: "high" },
    CatalogEntry { model: "anthropic/claude-sonnet-5-5", tier: "strong", pool: "claude", cost: 15.0, thinking: "high" },
    CatalogEntry { model: "openai-codex/gpt-5.3-codex", tier: "strong", pool: "codex", cost: 15.0, thinking: "high" },
    CatalogEntry { model: "antigravity/gemini-4-neon", tier: "strong", pool: "antigravity-gemini", cost: 15.0, thinking: "high" },
    CatalogEntry { model: "antigravity/claude-sonnet-5-5", tier: "strong", pool: "antigravity-claude-gpt", cost: 15.0, thinking: "high" },
    CatalogEntry { model: "cursor/grok-4.7", tier: "strong", pool: "cursor-third-party", cost: 12.0, thinking: "high" },
    CatalogEntry { model: "anthropic/claude-sonnet-5-5", tier: "balanced", pool: "claude", cost: 15.0, thinking: "medium" },
    CatalogEntry { model: "openai-codex/gpt-6-luna", tier: "balanced", pool: "codex", cost: 8.0, thinking: "medium" },
    CatalogEntry { model: "antigravity/gemini-4-argon", tier: "balanced", pool: "antigravity-gemini", cost: 10.0, thinking: "medium" },
    CatalogEntry { model: "antigravity/gpt-oss-120b", tier: "balanced", pool: "antigravity-claude-gpt", cost: 8.0, thinking: "medium" },
    CatalogEntry { model: "cursor/composer-2.5", tier: "balanced", pool: "cursor", cost: 6.0, thinking: "medium" },
    CatalogEntry { model: "anthropic/claude-haiku-4-5", tier: "fast", pool: "claude", cost: 5.0, thinking: "low" },
    CatalogEntry { model: "openai-codex/gpt-5.4-mini", tier: "fast", pool: "codex", cost: 2.0, thinking: "low" },
    CatalogEntry { model: "antigravity/gemini-3.8-flash", tier: "fast", pool: "antigravity-gemini", cost: 3.0, thinking: "low" },
    CatalogEntry { model: "cursor/composer-2.5-fast", tier: "fast", pool: "cursor", cost: 6.0, thinking: "low" },
];

const EFFICIENT_CHOICES: &[&str] = &[
    "antigravity/gemini-3.8-flash",
    "openai-codex/gpt-5.4-mini",
    "anthropic/claude-haiku-4-5",
    "cursor/composer-2.5",
    "cursor/composer-2.5-fast",
];

const CAPABLE_CHOICES: &[&str] = &[
    "antigravity/gemini-4-neon",
    "anthropic/claude-opus-5-5",
    "openai-codex/gpt-6.1-sol",
    "openai-codex/gpt-5.5",
    "antigravity/claude-opus-5-5",
    "cursor/grok-4.7",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterProfileDraft {
    pub routing_mode: String,
    pub preset_slug: String,
    pub provider_priority: String,
    pub cost_efficient: bool,
    pub switchyard_enabled: bool,
    pub efficient_model: String,
    pub capable_model: String,
    pub sensitivity: String,
}

impl Default for RouterProfileDraft {
    fn default() -> Self {
        let base = PRESETS[0];
        Self {
            routing_mode: "auto".to_string(),
            preset_slug: base.slug.to_string(),
            provider_priority: base.priority.to_string(),
            cost_efficient: base.cost_efficient,
            switchyard_enabled: base.switchyard,
            efficient_model: base.efficient_model.to_string(),
            capable_model: base.capable_model.to_string(),
            sensitivity: base.sensitivity.to_string(),
        }
    }
}

impl RouterProfileDraft {
    fn apply_preset(&mut self, spec: PresetSpec) {
        self.preset_slug = spec.slug.to_string();
        self.cost_efficient = spec.cost_efficient;
        self.provider_priority = spec.priority.to_string();
        self.switchyard_enabled = spec.switchyard;
        self.efficient_model = spec.efficient_model.to_string();
        self.capable_model = spec.capable_model.to_string();
        self.sensitivity = spec.sensitivity.to_string();
    }

    pub fn validate_switchyard(&self) -> Option<String> {
        if !self.switchyard_enabled {
            return None;
        }
        if self.efficient_model == self.capable_model {
            return Some("Choose different efficient and capable models.".to_string());
        }
        let low = CATALOG.iter().find(|c| c.model == self.efficient_model);
        let high = CATALOG.iter().find(|c| c.model == self.capable_model);
        if let (Some(low), Some(high)) = (low, high) {
            if high.cost < 2.0 * low.cost {
                return Some(
                    "Capable model must cost at least 2\u{d7} the efficient model on output tokens."
                        .to_string(),
                );
            }
        }
        None
    }

    pub fn to_json(&self) -> Value {
        let preset_label = PRESETS
            .iter()
            .find(|p| p.slug == self.preset_slug)
            .map_or("Coding agents", |p| p.label);
        json!({
            "name": preset_label,
            "preset_type": self.preset_slug,
            "routing_mode": self.routing_mode,
            "cost_efficient_routing_enabled": self.cost_efficient,
            "provider_routing_preference": self.provider_priority,
            "switchyard_routing_enabled": self.switchyard_enabled,
            "switchyard_settings": {
                "efficient_model": self.efficient_model,
                "capable_model": self.capable_model,
                "sensitivity_config_name": self.sensitivity,
            }
        })
    }

    pub fn from_json(value: &Value) -> Self {
        let mut draft = Self::default();
        let Some(obj) = value.as_object() else {
            return draft;
        };
        if let Some(slug) = obj
            .get("preset_type")
            .or_else(|| obj.get("slug"))
            .and_then(Value::as_str)
        {
            if let Some(spec) = PRESETS.iter().find(|p| p.slug == slug) {
                draft.apply_preset(*spec);
            }
        }
        if let Some(mode) = obj.get("routing_mode").and_then(Value::as_str) {
            if MODES.iter().any(|(k, _)| *k == mode) {
                draft.routing_mode = mode.to_string();
            }
        }
        if let Some(prio) = obj
            .get("provider_routing_preference")
            .or_else(|| obj.get("provider_priority"))
            .and_then(Value::as_str)
        {
            if PRIORITIES.iter().any(|(k, _)| *k == prio) {
                draft.provider_priority = prio.to_string();
            }
        }
        if let Some(cost_eff) = obj
            .get("cost_efficient_routing_enabled")
            .or_else(|| obj.get("cost_efficient"))
            .and_then(Value::as_bool)
        {
            draft.cost_efficient = cost_eff;
        }
        if let Some(sw) = obj.get("switchyard_routing_enabled").and_then(Value::as_bool) {
            draft.switchyard_enabled = sw;
        }
        if let Some(sw_obj) = obj.get("switchyard_settings").and_then(Value::as_object) {
            if let Some(eff) = sw_obj.get("efficient_model").and_then(Value::as_str) {
                if !eff.is_empty() {
                    draft.efficient_model = eff.to_string();
                }
            }
            if let Some(cap) = sw_obj.get("capable_model").and_then(Value::as_str) {
                if !cap.is_empty() {
                    draft.capable_model = cap.to_string();
                }
            }
            if let Some(sens) = sw_obj
                .get("sensitivity_config_name")
                .or_else(|| sw_obj.get("sensitivity"))
                .and_then(Value::as_str)
            {
                if SENSITIVITIES.iter().any(|(k, _)| *k == sens) {
                    draft.sensitivity = sens.to_string();
                }
            }
        }
        draft
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PoolSnapshot {
    pub pool: String,
    pub used_percent: f64,
    pub circuit_open: bool,
    pub latency_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterViewAction {
    None,
    Refresh,
    Saved,
    Close,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouterView {
    home_dir: PathBuf,
    saved: RouterProfileDraft,
    draft: RouterProfileDraft,
    selected: usize,
    pools: Vec<PoolSnapshot>,
    ramp_gateway: Option<String>,
    refreshing: bool,
    notice: Option<String>,
    error: Option<String>,
}

const ROW_COUNT: usize = 8;

impl RouterView {
    #[must_use]
    pub fn open(report: Option<&UsageReport>, refreshing: bool) -> Self {
        Self::open_in(default_harness_dir(), report, refreshing)
    }

    #[must_use]
    pub fn open_in(home_dir: PathBuf, report: Option<&UsageReport>, refreshing: bool) -> Self {
        let saved = load_profile(&home_dir.join(PROFILE_FILENAME));
        let mut view = Self {
            saved: saved.clone(),
            draft: saved,
            selected: 0,
            pools: Vec::new(),
            ramp_gateway: detect_ramp_gateway(),
            refreshing,
            notice: None,
            error: None,
            home_dir,
        };
        view.reload_telemetry(report);
        view
    }

    pub fn set_refreshing(&mut self) {
        self.refreshing = true;
    }

    pub fn reload_telemetry(&mut self, report: Option<&UsageReport>) {
        self.refreshing = false;
        let providers = match report {
            Some(UsageReport::Loaded(list)) => Some(list.clone()),
            _ => load_cached_providers(&self.home_dir.join(QUOTA_CACHE_FILENAME)),
        };
        let health = load_health_map(&self.home_dir.join(HEALTH_FILENAME));
        self.pools = build_pool_snapshots(providers.as_deref(), &health);
    }

    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.draft != self.saved
    }

    pub fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> RouterViewAction {
        if key == "ctrl+c"
            || key == "q"
            || kb.matches(key, "tui.select.cancel")
            || kb.matches(key, "app.modal.back")
        {
            return RouterViewAction::Close;
        }
        if key == "r" {
            self.notice = Some("Refreshing quotas\u{2026}".to_string());
            self.refreshing = true;
            return RouterViewAction::Refresh;
        }
        if key == "s" {
            return self.save();
        }
        if key == "d" {
            self.draft = self.saved.clone();
            self.error = None;
            self.notice = Some("Discarded unsaved changes.".to_string());
            return RouterViewAction::None;
        }
        if key == "k" || kb.matches(key, "tui.select.up") {
            self.selected = self.selected.saturating_sub(1);
            return RouterViewAction::None;
        }
        if key == "j" || kb.matches(key, "tui.select.down") {
            self.selected = (self.selected + 1).min(ROW_COUNT - 1);
            return RouterViewAction::None;
        }
        if key == "left" || key == "h" {
            self.step_selected(-1);
            return RouterViewAction::None;
        }
        if key == "right"
            || key == "l"
            || key == " "
            || key == "enter"
            || kb.matches(key, "tui.select.confirm")
        {
            self.step_selected(1);
            return RouterViewAction::None;
        }
        RouterViewAction::None
    }

    fn step_selected(&mut self, delta: isize) {
        self.notice = None;
        self.error = None;
        match self.selected {
            0 => {
                let keys: Vec<&str> = MODES.iter().map(|(k, _)| *k).collect();
                self.draft.routing_mode = cycle_str(&keys, &self.draft.routing_mode, delta);
            }
            1 => {
                let idx = PRESETS
                    .iter()
                    .position(|p| p.slug == self.draft.preset_slug)
                    .unwrap_or(0);
                let next = (idx as isize + delta).rem_euclid(PRESETS.len() as isize) as usize;
                self.draft.apply_preset(PRESETS[next]);
            }
            2 => {
                let keys: Vec<&str> = PRIORITIES.iter().map(|(k, _)| *k).collect();
                self.draft.provider_priority =
                    cycle_str(&keys, &self.draft.provider_priority, delta);
            }
            3 => {
                self.draft.cost_efficient = !self.draft.cost_efficient;
            }
            4 => {
                self.draft.switchyard_enabled = !self.draft.switchyard_enabled;
            }
            5 => {
                self.draft.efficient_model =
                    cycle_str(EFFICIENT_CHOICES, &self.draft.efficient_model, delta);
            }
            6 => {
                self.draft.capable_model =
                    cycle_str(CAPABLE_CHOICES, &self.draft.capable_model, delta);
            }
            7 => {
                let keys: Vec<&str> = SENSITIVITIES.iter().map(|(k, _)| *k).collect();
                self.draft.sensitivity = cycle_str(&keys, &self.draft.sensitivity, delta);
            }
            _ => {}
        }
        if let Some(err) = self.draft.validate_switchyard() {
            self.error = Some(err);
        }
    }

    fn save(&mut self) -> RouterViewAction {
        if let Some(err) = self.draft.validate_switchyard() {
            self.error = Some(err);
            return RouterViewAction::None;
        }
        let path = self.home_dir.join(PROFILE_FILENAME);
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                self.error = Some(format!("Could not create router directory: {err}"));
                return RouterViewAction::None;
            }
        }
        let payload = match serde_json::to_string_pretty(&self.draft.to_json()) {
            Ok(text) => format!("{text}\n"),
            Err(err) => {
                self.error = Some(format!("Could not encode profile: {err}"));
                return RouterViewAction::None;
            }
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(err) = std::fs::write(&tmp, payload).and_then(|()| std::fs::rename(&tmp, &path))
        {
            self.error = Some(format!("Could not save router profile: {err}"));
            return RouterViewAction::None;
        }
        self.saved = self.draft.clone();
        self.error = None;
        self.notice = Some(format!("Saved to {}", path.display()));
        RouterViewAction::Saved
    }

    pub fn render(
        &mut self,
        theme: &Theme,
        width: usize,
        kb: &KeybindingsManager,
        viewport_rows: usize,
    ) -> Vec<Line> {
        let width = width.max(1);
        let viewport_rows = viewport_rows.max(1);
        let mut lines: Vec<Line> = Vec::with_capacity(viewport_rows);
        lines.push(Vec::new());

        // Title bar
        let status = if self.refreshing {
            "refreshing\u{2026}".to_string()
        } else if self.is_dirty() {
            "unsaved changes (press s to save)".to_string()
        } else if let Some(gw) = &self.ramp_gateway {
            format!("Ramp gateway: {gw}")
        } else {
            "saved".to_string()
        };
        let mut title: Line = vec![
            Span::raw("  "),
            theme.bold(theme.fg_span(ThemeColor::Text, "Model Router")),
            Span::raw("  "),
            theme.fg_span(ThemeColor::Dim, "Ramp V2"),
        ];
        let left_w: usize = title.iter().map(|s| str_width(&s.content)).sum();
        let status_w = str_width(&status);
        let gap = if left_w + 2 + status_w + 2 <= width {
            width - left_w - status_w - 2
        } else {
            2
        };
        title.push(Span::raw(" ".repeat(gap)));
        title.push(theme.fg_span(
            if self.is_dirty() {
                ThemeColor::Warning
            } else {
                ThemeColor::Muted
            },
            status,
        ));
        lines.push(truncate_line(&title, width, "\u{2026}"));

        let rule_w = width.saturating_sub(4).max(1);
        lines.push(vec![
            Span::raw(if width >= 4 { "  " } else { "" }),
            theme.fg_span(ThemeColor::BorderMuted, "\u{2500}".repeat(rule_w)),
        ]);
        lines.push(Vec::new());

        // Summary strip on UserMessageBg
        lines.push(self.render_summary_strip(theme, width));
        lines.push(Vec::new());

        // Section 1: Routing Strategy controls
        lines.push(vec![
            Span::raw("  "),
            theme.bold(theme.fg_span(ThemeColor::Accent, "Routing Strategy")),
        ]);
        let controls: [(&str, String); ROW_COUNT] = [
            ("Router mode", lookup_label(MODES, &self.draft.routing_mode)),
            (
                "Workload preset",
                PRESETS
                    .iter()
                    .find(|p| p.slug == self.draft.preset_slug)
                    .map_or(self.draft.preset_slug.as_str(), |p| p.label)
                    .to_string(),
            ),
            (
                "Provider priority",
                lookup_label(PRIORITIES, &self.draft.provider_priority),
            ),
            (
                "Cost-efficient",
                if self.draft.cost_efficient {
                    "Enabled".to_string()
                } else {
                    "Disabled".to_string()
                },
            ),
            (
                "Switchyard 2-tier",
                if self.draft.switchyard_enabled {
                    "Enabled".to_string()
                } else {
                    "Disabled".to_string()
                },
            ),
            ("  Efficient model", self.draft.efficient_model.clone()),
            ("  Capable model", self.draft.capable_model.clone()),
            (
                "  Sensitivity",
                lookup_label(SENSITIVITIES, &self.draft.sensitivity),
            ),
        ];
        for (idx, (label, val)) in controls.iter().enumerate() {
            let selected = idx == self.selected;
            let dimmed = idx >= 5 && !self.draft.switchyard_enabled;
            let prefix = if selected { "  \u{25b8} " } else { "    " };
            let label_pad = 22usize.saturating_sub(str_width(label));
            let row = if selected {
                let bg = theme.bg_style(ThemeBg::UserMessageBg);
                let accent = bg.patch(theme.fg_style(ThemeColor::Accent)).add_modifier(Modifier::BOLD);
                let text = bg.patch(theme.fg_style(ThemeColor::Text)).add_modifier(Modifier::BOLD);
                let muted = bg.patch(theme.fg_style(ThemeColor::Muted));
                let mut spans = vec![
                    Span::styled(prefix, accent),
                    Span::styled(format!("{label}{}", " ".repeat(label_pad)), text),
                    Span::styled("  \u{25c2} ", muted),
                    Span::styled(val.clone(), text),
                    Span::styled(" \u{25b8}", muted),
                ];
                let used: usize = spans.iter().map(|s| str_width(&s.content)).sum();
                let fill = width.saturating_sub(used + 2);
                if fill > 0 {
                    spans.push(Span::styled(" ".repeat(fill), bg));
                }
                spans
            } else {
                let label_color = if dimmed { ThemeColor::Dim } else { ThemeColor::Muted };
                let val_color = if dimmed { ThemeColor::Dim } else { ThemeColor::Text };
                vec![
                    Span::raw(prefix),
                    theme.fg_span(label_color, format!("{label}{}", " ".repeat(label_pad))),
                    Span::raw("    "),
                    theme.fg_span(val_color, val.clone()),
                ]
            };
            lines.push(truncate_line(&row, width, "\u{2026}"));
        }

        if let Some(err) = &self.error {
            lines.push(vec![
                Span::raw("    "),
                theme.fg_span(ThemeColor::Error, err.clone()),
            ]);
        } else if let Some(note) = &self.notice {
            lines.push(vec![
                Span::raw("    "),
                theme.fg_span(ThemeColor::Success, note.clone()),
            ]);
        }

        // Section 2: Live Tier Preview
        lines.push(Vec::new());
        lines.push(vec![
            Span::raw("  "),
            theme.bold(theme.fg_span(ThemeColor::Accent, "Live Tier Preview")),
        ]);
        for tier in ["fast", "balanced", "strong", "frontier"] {
            let (model, thinking, note) = self.preview_tier(tier);
            let mut row = vec![
                Span::raw("    "),
                theme.fg_span(ThemeColor::Muted, format!("{tier:<10}")),
                Span::raw("  "),
                theme.fg_span(
                    if model == "none healthy" {
                        ThemeColor::Error
                    } else {
                        ThemeColor::Text
                    },
                    model,
                ),
            ];
            if !thinking.is_empty() {
                row.push(Span::raw("  "));
                row.push(theme.fg_span(ThemeColor::Dim, thinking));
            }
            if !note.is_empty() {
                row.push(Span::raw("  "));
                row.push(theme.fg_span(ThemeColor::Dim, note));
            }
            lines.push(truncate_line(&row, width, "\u{2026}"));
        }

        // Section 3: Live Pool Headroom
        if !self.pools.is_empty() {
            lines.push(Vec::new());
            lines.push(vec![
                Span::raw("  "),
                theme.bold(theme.fg_span(ThemeColor::Accent, "Pool Headroom")),
            ]);
            let bar_w = if width >= 70 { 18 } else { 12 };
            for pool in &self.pools {
                let (filled, empty) = bar_cells_width(pool.used_percent, bar_w);
                let color = if pool.circuit_open {
                    ThemeColor::Error
                } else {
                    bar_color(pool.used_percent)
                };
                let label = truncate_to_width(&pool.pool, 22, "\u{2026}");
                let pad = 22usize.saturating_sub(str_width(&label));
                let mut row = vec![
                    Span::raw("    "),
                    theme.fg_span(color, "\u{25cf}"),
                    Span::raw(" "),
                    theme.fg_span(ThemeColor::Muted, format!("{label}{}", " ".repeat(pad))),
                    Span::raw("  "),
                    theme.fg_span(color, filled),
                    theme.fg_span(ThemeColor::BorderMuted, empty),
                    Span::raw("  "),
                    theme.fg_span(
                        if pool.used_percent >= 85.0 {
                            ThemeColor::Error
                        } else {
                            ThemeColor::Text
                        },
                        format!("{:>3.0}%", pool.used_percent.clamp(0.0, 999.0)),
                    ),
                ];
                if pool.circuit_open {
                    row.push(Span::raw("  "));
                    row.push(theme.fg_span(ThemeColor::Error, "circuit open"));
                }
                lines.push(truncate_line(&row, width, "\u{2026}"));
            }
        }

        let footer_rows = 2;
        if lines.len() + footer_rows > viewport_rows {
            lines.truncate(viewport_rows.saturating_sub(footer_rows));
        }
        while lines.len() + footer_rows < viewport_rows {
            lines.push(Vec::new());
        }
        let close = kb.first_key("tui.select.cancel").map_or_else(
            || "Esc".to_string(),
            |key| crate::keybindings::format_key_text(&key),
        );
        lines.push(hint_row(
            theme,
            width,
            &format!(
                "\u{2191}\u{2193} select \u{b7} \u{2190}\u{2192}/Enter change \u{b7} s save \u{b7} d discard \u{b7} r refresh \u{b7} {close} close"
            ),
        ));
        lines.push(Vec::new());
        lines.truncate(viewport_rows);
        lines
    }

    fn render_summary_strip(&self, theme: &Theme, width: usize) -> Line {
        let muted = theme.fg_style(ThemeColor::Muted);
        let value = theme.fg_style(ThemeColor::Text).add_modifier(Modifier::BOLD);
        let dim = theme.fg_style(ThemeColor::Dim);
        let mode_short = match self.draft.routing_mode.as_str() {
            "on" => "Active",
            "off" => "Off",
            _ => "Auto",
        };
        let prio_short = lookup_label(PRIORITIES, &self.draft.provider_priority);
        let sw_short = if self.draft.switchyard_enabled {
            format!("On ({})", self.draft.sensitivity)
        } else {
            "Off".to_string()
        };
        let spans = vec![
            Span::raw("  "),
            Span::styled("Mode ", muted),
            Span::styled(mode_short, value),
            Span::styled("   \u{b7}   ", dim),
            Span::styled("Priority ", muted),
            Span::styled(prio_short, value),
            Span::styled("   \u{b7}   ", dim),
            Span::styled("Switchyard ", muted),
            Span::styled(sw_short, value),
        ];
        truncate_line(&spans, width, "\u{2026}")
    }

    fn pool_ok(&self, pool: &str, tier: &str) -> (bool, f64, f64) {
        let limit = if matches!(tier, "strong" | "frontier") {
            98.0
        } else {
            90.0
        };
        let snap = self.pools.iter().find(|p| p.pool == pool);
        let cursor_blocked = pool.starts_with("cursor")
            && self
                .pools
                .iter()
                .find(|p| p.pool == "cursor-total")
                .is_some_and(|p| p.circuit_open || p.used_percent >= limit);
        if cursor_blocked {
            return (false, 100.0, 0.0);
        }
        match snap {
            Some(p) => (!p.circuit_open && p.used_percent < limit, p.used_percent, p.latency_ms),
            None => (true, 0.0, 0.0),
        }
    }

    fn preview_tier(&self, tier: &str) -> (String, String, String) {
        if self.draft.routing_mode == "off" {
            return ("pinned model".to_string(), String::new(), "router off".to_string());
        }
        let thinking = match tier {
            "fast" => "low",
            "balanced" => "medium",
            "strong" => "high",
            _ => "xhigh",
        };
        if self.draft.switchyard_enabled && self.draft.validate_switchyard().is_none() {
            let use_capable = match self.draft.sensitivity.as_str() {
                "frugal" => tier == "frontier",
                "quality" => tier != "fast",
                _ => matches!(tier, "strong" | "frontier"),
            };
            let target = if use_capable {
                &self.draft.capable_model
            } else {
                &self.draft.efficient_model
            };
            if let Some(entry) = CATALOG.iter().find(|c| c.model == target) {
                let (ok, _, _) = self.pool_ok(entry.pool, tier);
                if ok {
                    let tag = if use_capable { "switchyard capable" } else { "switchyard efficient" };
                    return (entry.model.to_string(), thinking.to_string(), tag.to_string());
                }
            }
        }
        let mut viable: Vec<(CatalogEntry, f64, f64)> = CATALOG
            .iter()
            .filter(|c| c.tier == tier)
            .filter_map(|&c| {
                let (ok, pressure, latency) = self.pool_ok(c.pool, tier);
                ok.then_some((c, pressure, latency))
            })
            .collect();
        if viable.is_empty() {
            return ("none healthy".to_string(), String::new(), "all pools full".to_string());
        }
        viable.sort_by(|(a, ap, al), (b, bp, bl)| {
            if matches!(
                self.draft.provider_priority.as_str(),
                "throughput_only" | "favor_throughput"
            ) {
                al.total_cmp(bl)
                    .then_with(|| ap.total_cmp(bp))
                    .then_with(|| a.cost.total_cmp(&b.cost))
                    .then_with(|| a.model.cmp(b.model))
            } else {
                a.cost
                    .total_cmp(&b.cost)
                    .then_with(|| ((ap / 25.0) as u32).cmp(&((bp / 25.0) as u32)))
                    .then_with(|| a.model.cmp(b.model))
            }
        });
        let (best, pressure, _) = viable[0];
        let note = if pressure > 0.0 {
            format!("{} {:.0}%", best.pool, pressure)
        } else {
            best.pool.to_string()
        };
        (best.model.to_string(), best.thinking.to_string(), note)
    }
}

fn cycle_str(choices: &[&str], current: &str, delta: isize) -> String {
    let idx = choices.iter().position(|c| *c == current).unwrap_or(0);
    let next = (idx as isize + delta).rem_euclid(choices.len() as isize) as usize;
    choices[next].to_string()
}

fn lookup_label(pairs: &[(&str, &str)], key: &str) -> String {
    pairs
        .iter()
        .find(|(k, _)| *k == key)
        .map_or_else(|| key.to_string(), |(_, label)| (*label).to_string())
}

fn default_harness_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PA_ROUTER_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".prime/harness")
}

fn load_profile(path: &Path) -> RouterProfileDraft {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .map(|val| RouterProfileDraft::from_json(&val))
        .unwrap_or_default()
}

fn load_cached_providers(path: &Path) -> Option<Vec<ProviderUsage>> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    let raw = doc.get("raw")?.as_str()?;
    parse_usage(raw).ok()
}

fn load_health_map(path: &Path) -> HashMap<String, (bool, f64)> {
    let mut out = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        return out;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    let Some(pools) = doc.get("pools").and_then(Value::as_object) else {
        return out;
    };
    for (pool, entry) in pools {
        let open_until = entry
            .get("open_until")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let latency = entry
            .get("latency_ewma_ms")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        out.insert(pool.clone(), (now < open_until, latency));
    }
    out
}

fn build_pool_snapshots(
    providers: Option<&[ProviderUsage]>,
    health: &HashMap<String, (bool, f64)>,
) -> Vec<PoolSnapshot> {
    let mut map: HashMap<String, f64> = HashMap::new();
    if let Some(list) = providers {
        for p in list {
            let name = p.provider.to_ascii_lowercase();
            match name.as_str() {
                "claude" | "codex" => {
                    let max_pct = p
                        .windows
                        .iter()
                        .map(|w| w.used_percent)
                        .fold(0.0_f64, f64::max);
                    map.insert(name, max_pct);
                }
                "antigravity" => {
                    let mut gem = 0.0_f64;
                    let mut cg = 0.0_f64;
                    let mut has_gem = false;
                    let mut has_cg = false;
                    for w in &p.windows {
                        let lower = w.label.to_ascii_lowercase();
                        if lower.contains("claude") || lower.contains("gpt") {
                            cg = cg.max(w.used_percent);
                            has_cg = true;
                        } else {
                            gem = gem.max(w.used_percent);
                            has_gem = true;
                        }
                    }
                    if has_gem {
                        map.insert("antigravity-gemini".to_string(), gem);
                    }
                    if has_cg {
                        map.insert("antigravity-claude-gpt".to_string(), cg);
                    }
                }
                "cursor" => {
                    for w in &p.windows {
                        let lower = w.label.to_ascii_lowercase();
                        if lower.contains("total") {
                            map.insert("cursor-total".to_string(), w.used_percent);
                        } else if lower.contains("third") {
                            map.insert("cursor-third-party".to_string(), w.used_percent);
                        } else if lower == "cursor" {
                            map.insert("cursor".to_string(), w.used_percent);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let order = [
        "claude",
        "codex",
        "antigravity-gemini",
        "antigravity-claude-gpt",
        "cursor-total",
        "cursor",
        "cursor-third-party",
    ];
    order
        .iter()
        .filter_map(|&pool| {
            let used = map.get(pool).copied()?;
            let (open, latency) = health.get(pool).copied().unwrap_or((false, 0.0));
            Some(PoolSnapshot {
                pool: pool.to_string(),
                used_percent: used,
                circuit_open: open,
                latency_ms: latency,
            })
        })
        .collect()
}

fn detect_ramp_gateway() -> Option<String> {
    if let (Ok(url), Ok(key)) = (
        std::env::var("ROUTER_BASE_URL"),
        std::env::var("ROUTER_API_KEY"),
    ) {
        if !url.trim().is_empty() && !key.trim().is_empty() {
            return Some("env".to_string());
        }
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let mut found = Vec::new();
    if home.join(".claude/ramp-router-state.json").is_file() {
        found.push("claude-code");
    }
    if home.join(".gemini/config/ramp-router-state.json").is_file() {
        found.push("antigravity");
    }
    (!found.is_empty()).then(|| found.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorMode;

    fn theme() -> Theme {
        Theme::builtin("prime", ColorMode::TrueColor)
    }

    fn kb() -> KeybindingsManager {
        KeybindingsManager::new()
    }

    fn text(line: &Line) -> String {
        line.iter().map(|s| s.content.as_str()).collect()
    }

    #[test]
    fn cycles_presets_saves_and_reloads_ramp_v2_profile() {
        let dir = tempfile::tempdir().unwrap();
        let mut view = RouterView::open_in(dir.path().to_path_buf(), None, false);
        assert!(!view.is_dirty());
        // Row 0 -> Mode: auto -> on
        assert_eq!(view.handle_key("right", &kb()), RouterViewAction::None);
        assert_eq!(view.draft.routing_mode, "on");
        assert!(view.is_dirty());
        // Row 1 -> Workload preset: coding-agents -> coding-agents-experimental (Switchyard on)
        view.handle_key("down", &kb());
        view.handle_key("right", &kb());
        assert_eq!(view.draft.preset_slug, "coding-agents-experimental");
        assert!(view.draft.switchyard_enabled);
        // Save and verify persistence
        assert_eq!(view.handle_key("s", &kb()), RouterViewAction::Saved);
        assert!(!view.is_dirty());
        let reloaded = RouterView::open_in(dir.path().to_path_buf(), None, false);
        assert_eq!(reloaded.draft.routing_mode, "on");
        assert_eq!(reloaded.draft.preset_slug, "coding-agents-experimental");
        assert!(reloaded.draft.switchyard_enabled);
    }

    #[test]
    fn renders_live_tier_preview_and_pool_headroom_within_width() {
        let dir = tempfile::tempdir().unwrap();
        let providers = parse_usage(
            r#"[
              {"provider":"claude","usage":{"primary":{"usedPercent":100,"windowMinutes":300}}},
              {"provider":"antigravity","usage":{"extraRateWindows":[
                {"title":"Gemini 5-hour","window":{"usedPercent":40,"windowMinutes":300}},
                {"title":"Claude/GPT 5-hour","window":{"usedPercent":100,"windowMinutes":300}}
              ]}}
            ]"#,
        )
        .unwrap();
        let report = UsageReport::Loaded(providers);
        let mut view = RouterView::open_in(dir.path().to_path_buf(), Some(&report), false);
        let rows: Vec<String> = view
            .render(&theme(), 90, &kb(), 36)
            .iter()
            .map(text)
            .collect();
        assert!(rows.iter().any(|r| r.contains("Model Router")));
        assert!(rows.iter().any(|r| r.contains("Routing Strategy")));
        assert!(rows.iter().any(|r| r.contains("Live Tier Preview")));
        assert!(rows.iter().any(|r| r.contains("antigravity-gemini")));
        assert!(rows.iter().any(|r| r.contains("antigravity/gemini-4-neon")));
        for row in &rows {
            assert!(str_width(row) <= 90, "{row:?}");
        }
    }

    #[test]
    fn switchyard_rejects_same_model_pair() {
        let mut draft = RouterProfileDraft::default();
        draft.switchyard_enabled = true;
        draft.efficient_model = "antigravity/gemini-4-neon".to_string();
        draft.capable_model = "antigravity/gemini-4-neon".to_string();
        assert!(draft.validate_switchyard().is_some());
    }
}
