//! Inline folder picker for adding existing project directories or creating new ones
//! directly from the F6 Captain panel.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::fuzzy::fuzzy_match;
use crate::keybindings::KeybindingsManager;
use crate::menu_panel::{menu_row, no_match_row, search_field_lines, MenuSegment};
use crate::theme::{Theme, ThemeColor};
use crate::width::{truncate_line, truncate_to_width};
use crate::{Line, Span};

use super::CaptainProject;

/// Upper bound on subdirectories scanned in one directory so huge folders never stall the UI.
const MAX_DIR_ENTRIES: usize = 2_000;

/// One subdirectory inside [`FolderPicker::current_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DirEntry {
    pub name: String,
    pub path: PathBuf,
    canonical_path: Option<PathBuf>,
    pub is_git: bool,
    pub is_hidden: bool,
    pub captain: Option<(String, String)>,
}

/// One selectable row in the inline folder picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FolderRow {
    /// Top row when the filter is empty: use `current_dir` (or open its existing Captain).
    UseCurrent {
        path: PathBuf,
        display: String,
        is_git: bool,
        /// `(project_id, project_name)` when `current_dir` already has an active/paused Captain.
        captain: Option<(String, String)>,
    },
    /// An existing child subdirectory of `current_dir`.
    Dir {
        name: String,
        path: PathBuf,
        is_git: bool,
        /// `(project_id, project_name)` when this folder already has an active/paused Captain.
        captain: Option<(String, String)>,
    },
    /// Bottom row when `filter` names a valid new directory that does not exist yet.
    CreateNew { name: String, path: PathBuf },
}

/// Result of handling one key in [`FolderPicker`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FolderPickResult {
    Stay,
    Cancel,
    OpenExisting { project_id: String, name: String },
    SelectFolder { root: String, create_dir: bool },
}

/// Inline directory browser state for `Mode::PickFolder`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FolderPicker {
    pub current_dir: PathBuf,
    current_dir_canonical: Option<PathBuf>,
    current_dir_display: String,
    current_dir_is_git: bool,
    current_dir_captain: Option<(String, String)>,
    pub entries: Vec<DirEntry>,
    /// Lowercased names of all entries (files and folders) in `current_dir` so `rows()` never hits the filesystem.
    existing_names: HashSet<String>,
    truncated: bool,
    captain_by_raw: HashMap<PathBuf, (String, String)>,
    captain_by_canonical: HashMap<PathBuf, (String, String)>,
    pub filter: String,
    pub selected: usize,
    pub read_error: Option<String>,
}

impl FolderPicker {
    pub(super) fn new(start_dir: PathBuf, projects: &[CaptainProject]) -> Self {
        let (captain_by_raw, captain_by_canonical) = index_captains(projects);
        let mut picker = Self {
            current_dir: start_dir,
            current_dir_canonical: None,
            current_dir_display: String::new(),
            current_dir_is_git: false,
            current_dir_captain: None,
            entries: Vec::new(),
            existing_names: HashSet::new(),
            truncated: false,
            captain_by_raw,
            captain_by_canonical,
            filter: String::new(),
            selected: 0,
            read_error: None,
        };
        picker.refresh_entries();
        picker
    }

    /// Refresh cached Captain root badges when a fresh snapshot arrives while the picker is open.
    pub(super) fn sync_projects(&mut self, projects: &[CaptainProject]) {
        let (by_raw, by_canonical) = index_captains(projects);
        self.captain_by_raw = by_raw;
        self.captain_by_canonical = by_canonical;
        self.current_dir_captain =
            self.lookup_captain(&self.current_dir, self.current_dir_canonical.as_deref());
        for entry in &mut self.entries {
            entry.captain = lookup_in_maps(
                &entry.path,
                entry.canonical_path.as_deref(),
                &self.captain_by_raw,
                &self.captain_by_canonical,
            );
        }
    }

    fn lookup_captain(&self, path: &Path, canonical: Option<&Path>) -> Option<(String, String)> {
        lookup_in_maps(
            path,
            canonical,
            &self.captain_by_raw,
            &self.captain_by_canonical,
        )
    }

    pub(super) fn refresh_entries(&mut self) {
        self.entries.clear();
        self.existing_names.clear();
        self.truncated = false;
        self.read_error = None;
        self.current_dir_display = display_path(&self.current_dir.to_string_lossy());
        self.current_dir_canonical = std::fs::canonicalize(&self.current_dir).ok();
        self.current_dir_is_git = self.current_dir.join(".git").exists();
        self.current_dir_captain =
            self.lookup_captain(&self.current_dir, self.current_dir_canonical.as_deref());

        let read_dir = match std::fs::read_dir(&self.current_dir) {
            Ok(rd) => rd,
            Err(err) => {
                self.read_error = Some(format!("Cannot read directory: {err}"));
                return;
            }
        };
        let mut dirs: Vec<DirEntry> = Vec::new();
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.is_empty() {
                continue;
            }
            self.existing_names.insert(name.to_ascii_lowercase());
            let path = entry.path();
            let (is_dir, is_symlink) = match entry.file_type() {
                Ok(ft) if ft.is_symlink() => (path.is_dir(), true),
                Ok(ft) => (ft.is_dir(), false),
                Err(_) => (path.is_dir(), false),
            };
            if !is_dir {
                continue;
            }
            if dirs.len() >= MAX_DIR_ENTRIES {
                self.truncated = true;
                break;
            }
            let is_hidden = name.starts_with('.');
            let is_git = !is_hidden && path.join(".git").exists();
            let canonical_path = if is_symlink {
                std::fs::canonicalize(&path).ok()
            } else {
                self.current_dir_canonical.as_ref().map(|c| c.join(&name))
            };
            let captain = self.lookup_captain(&path, canonical_path.as_deref());
            dirs.push(DirEntry {
                name,
                path,
                canonical_path,
                is_git,
                is_hidden,
                captain,
            });
        }
        dirs.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.name.cmp(&b.name))
        });
        self.entries = dirs;
    }

    pub(super) fn enter_dir(&mut self, dir: PathBuf) {
        self.current_dir = dir;
        self.filter.clear();
        self.selected = 0;
        self.refresh_entries();
    }

    pub(super) fn go_up(&mut self) {
        if let Some(parent) = self.current_dir.parent() {
            if parent != self.current_dir {
                self.enter_dir(parent.to_path_buf());
            }
        }
    }

    /// Build the visible rows purely from cached state (zero filesystem I/O).
    pub(super) fn rows(&self) -> Vec<FolderRow> {
        let mut out: Vec<FolderRow> = Vec::new();
        let show_hidden = self.filter.starts_with('.');
        let filter_trimmed = self.filter.trim();

        if self.filter.is_empty() {
            out.push(FolderRow::UseCurrent {
                path: self.current_dir.clone(),
                display: self.current_dir_display.clone(),
                is_git: self.current_dir_is_git,
                captain: self.current_dir_captain.clone(),
            });
            for entry in self.entries.iter().filter(|e| !e.is_hidden) {
                out.push(FolderRow::Dir {
                    name: entry.name.clone(),
                    path: entry.path.clone(),
                    is_git: entry.is_git,
                    captain: entry.captain.clone(),
                });
            }
            return out;
        }

        let mut matched: Vec<(&DirEntry, f64)> = self
            .entries
            .iter()
            .filter(|e| show_hidden || !e.is_hidden)
            .filter_map(|e| fuzzy_match(&self.filter, &e.name).map(|score| (e, score)))
            .collect();
        matched.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        for (entry, _) in matched {
            out.push(FolderRow::Dir {
                name: entry.name.clone(),
                path: entry.path.clone(),
                is_git: entry.is_git,
                captain: entry.captain.clone(),
            });
        }

        if is_valid_new_folder_name(filter_trimmed)
            && !self
                .existing_names
                .contains(&filter_trimmed.to_ascii_lowercase())
        {
            out.push(FolderRow::CreateNew {
                name: filter_trimmed.to_owned(),
                path: self.current_dir.join(filter_trimmed),
            });
        }

        out
    }

    pub(super) fn handle_key(
        &mut self,
        key: &str,
        kb: &KeybindingsManager,
    ) -> FolderPickResult {
        if kb.matches(key, "tui.select.cancel") || key == "ctrl+c" {
            return FolderPickResult::Cancel;
        }
        let rows = self.rows();
        if kb.matches(key, "tui.select.up") || key == "up" || key == "ctrl+p" {
            self.selected = self.selected.saturating_sub(1);
            return FolderPickResult::Stay;
        }
        if kb.matches(key, "tui.select.down") || key == "down" || key == "ctrl+n" {
            self.selected = (self.selected + 1).min(rows.len().saturating_sub(1));
            return FolderPickResult::Stay;
        }
        if key == "right" || key == "tab" {
            let idx = self.selected.min(rows.len().saturating_sub(1));
            if let Some(FolderRow::Dir { path, .. }) = rows.get(idx) {
                self.enter_dir(path.clone());
            }
            return FolderPickResult::Stay;
        }
        if key == "left" {
            self.go_up();
            return FolderPickResult::Stay;
        }
        if key == "backspace" {
            if self.filter.is_empty() {
                self.go_up();
            } else {
                self.filter.pop();
                self.selected = 0;
            }
            return FolderPickResult::Stay;
        }
        if kb.matches(key, "tui.select.confirm") || key == "enter" {
            let idx = self.selected.min(rows.len().saturating_sub(1));
            let Some(row) = rows.get(idx) else {
                return FolderPickResult::Stay;
            };
            return match row {
                FolderRow::UseCurrent {
                    captain: Some((id, name)),
                    ..
                }
                | FolderRow::Dir {
                    captain: Some((id, name)),
                    ..
                } => FolderPickResult::OpenExisting {
                    project_id: id.clone(),
                    name: name.clone(),
                },
                FolderRow::UseCurrent { path, .. } | FolderRow::Dir { path, .. } => {
                    FolderPickResult::SelectFolder {
                        root: path.to_string_lossy().into_owned(),
                        create_dir: false,
                    }
                }
                FolderRow::CreateNew { path, .. } => FolderPickResult::SelectFolder {
                    root: path.to_string_lossy().into_owned(),
                    create_dir: true,
                },
            };
        }
        if key == "space" {
            self.filter.push(' ');
            self.selected = 0;
            return FolderPickResult::Stay;
        }
        let mut chars = key.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if !c.is_control() {
                self.filter.push(c);
                self.selected = 0;
            }
        } else if let Some(rest) = key.strip_prefix("shift+") {
            if rest.chars().count() == 1 {
                self.filter.push_str(&rest.to_uppercase());
                self.selected = 0;
            }
        }
        FolderPickResult::Stay
    }

    pub(super) fn render(
        &self,
        theme: &Theme,
        width: usize,
        rows_budget: usize,
    ) -> Vec<Line> {
        let mut out: Vec<Line> = Vec::new();
        let visible_dir_count = self
            .entries
            .iter()
            .filter(|e| self.filter.starts_with('.') || !e.is_hidden)
            .count();
        let count_label = if self.truncated {
            format!("{visible_dir_count}+ folders")
        } else {
            format!(
                "{visible_dir_count} folder{}",
                if visible_dir_count == 1 { "" } else { "s" }
            )
        };
        out.push(truncate_line(
            &vec![
                Span::raw(" "),
                theme.bold(theme.fg_span(ThemeColor::Text, "add project")),
                theme.fg_span(ThemeColor::Dim, "  "),
                theme.fg_span(ThemeColor::Accent, self.current_dir_display.clone()),
                theme.fg_span(ThemeColor::Dim, format!("  · {count_label}")),
            ],
            width,
            "…",
        ));
        out.extend(search_field_lines(
            theme,
            width,
            &self.filter,
            self.filter.chars().count(),
            true,
            "Type to filter or name a new folder",
        ));

        if let Some(err) = &self.read_error {
            out.push(vec![
                Span::raw("   "),
                theme.fg_span(
                    ThemeColor::Error,
                    truncate_to_width(err, width.saturating_sub(4), "…"),
                ),
            ]);
        }

        let visible = self.rows();
        if visible.is_empty() {
            out.push(no_match_row(theme, width, "No matching folders"));
            return out;
        }

        let room = rows_budget.saturating_sub(out.len()).max(1);
        let selected = self.selected.min(visible.len().saturating_sub(1));
        let top = selected.saturating_sub(room - 1);

        for (offset, row) in visible.iter().enumerate().skip(top).take(room) {
            let is_selected = offset == selected;
            match row {
                FolderRow::UseCurrent {
                    display,
                    is_git,
                    captain,
                    ..
                } => {
                    let (primary, trailing): (Line, Vec<MenuSegment<'_>>) = match captain {
                        Some((_, name)) => (
                            vec![
                                theme.fg_span(ThemeColor::Success, "● "),
                                theme.fg_span(
                                    ThemeColor::Text,
                                    format!("Open existing Captain ({name})"),
                                ),
                            ],
                            vec![MenuSegment::muted(display)],
                        ),
                        None => {
                            let trailing = if *is_git {
                                vec![MenuSegment::themed(ThemeColor::Muted, "git")]
                            } else {
                                Vec::new()
                            };
                            (
                                vec![
                                    theme.fg_span(ThemeColor::Accent, "● "),
                                    theme.fg_span(
                                        ThemeColor::Text,
                                        format!("Use this folder ({display})"),
                                    ),
                                ],
                                trailing,
                            )
                        }
                    };
                    out.push(menu_row(theme, width, primary, &trailing, is_selected));
                }
                FolderRow::Dir {
                    name,
                    is_git,
                    captain,
                    ..
                } => {
                    let primary = vec![
                        theme.fg_span(ThemeColor::Text, name.clone()),
                        theme.fg_span(ThemeColor::Dim, "/"),
                    ];
                    let badge = captain.as_ref().map(|(_, cname)| format!("[Captain: {cname}]"));
                    let mut trailing: Vec<MenuSegment<'_>> = Vec::new();
                    if *is_git {
                        trailing.push(MenuSegment::themed(ThemeColor::Muted, "git"));
                    }
                    if let Some(ref b) = badge {
                        trailing.push(MenuSegment::themed(ThemeColor::Success, b));
                    }
                    out.push(menu_row(theme, width, primary, &trailing, is_selected));
                }
                FolderRow::CreateNew { name, .. } => {
                    let primary = vec![
                        theme.fg_span(ThemeColor::Success, "+ "),
                        theme.fg_span(
                            ThemeColor::Text,
                            format!("Create new folder \"{name}\" here"),
                        ),
                    ];
                    let trailing = [MenuSegment::themed(ThemeColor::Accent, "new folder")];
                    out.push(menu_row(theme, width, primary, &trailing, is_selected));
                }
            }
        }
        out
    }
}

/// Whether `path` exists and can be listed.
pub(super) fn is_readable_dir(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok()
}

/// Format an absolute path with `~` when under `$HOME`.
pub(super) fn display_path(raw: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home_path = Path::new(&home);
        let path = Path::new(raw);
        if let Ok(rel) = path.strip_prefix(home_path) {
            if rel.as_os_str().is_empty() {
                return "~".into();
            }
            return format!("~/{}", rel.to_string_lossy());
        }
    }
    raw.to_owned()
}

/// Whether `name` is a safe single-segment directory name to create under `current_dir`.
fn is_valid_new_folder_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// Index active/paused/stopping Captain project roots by both raw and canonical paths once.
fn index_captains(
    projects: &[CaptainProject],
) -> (
    HashMap<PathBuf, (String, String)>,
    HashMap<PathBuf, (String, String)>,
) {
    let mut by_raw = HashMap::new();
    let mut by_canonical = HashMap::new();
    for p in projects {
        if p.root.is_empty() || matches!(p.state.as_str(), "deleting" | "archived") {
            continue;
        }
        let val = (p.id.clone(), p.name.clone());
        let raw = PathBuf::from(&p.root);
        if let Ok(canon) = std::fs::canonicalize(&raw) {
            by_canonical.entry(canon).or_insert_with(|| val.clone());
        }
        by_raw.entry(raw).or_insert(val);
    }
    (by_raw, by_canonical)
}

fn lookup_in_maps(
    path: &Path,
    canonical: Option<&Path>,
    by_raw: &HashMap<PathBuf, (String, String)>,
    by_canonical: &HashMap<PathBuf, (String, String)>,
) -> Option<(String, String)> {
    by_raw
        .get(path)
        .or_else(|| canonical.and_then(|c| by_canonical.get(c)))
        .cloned()
}
