//! The frame compose: the fullscreen frame (top bar, padded transcript
//! window, dock at the bottom), the inline exit layout, the hardware-
//! cursor query, and the compose free helpers.

use super::click::{
    self, PickerClickSurface, PickerKind, CHOICE_PICKER_CHROME_ROWS, MODEL_PICKER_CHROME_ROWS,
};
use super::FULLSCREEN_MIN_TRANSCRIPT_ROWS;
use super::{AgentView, Overlay};
use crate::chrome::{render_prompt_context, render_top_bar};
use crate::theme::{Theme, ThemeColor};
use crate::width::str_width;
use crate::{Line, Span};
use ratatui::style::{Modifier, Style};

impl AgentView {
    /// Compose the fullscreen frame: top bar, transcript window, dock —
    /// exactly `height` rows.
    pub fn render_frame(&mut self, width: usize, height: usize) -> Vec<Line> {
        // The fullscreen compose forces image components to their textual fallback: re-emitting
        // an image placement each repaint would corrupt the display.
        let frame = crate::image_component::with_fullscreen_image_fallback(|| {
            self.render_frame_inner(width, height)
        });
        // The composed frame is the click surface (TS `hyperlinkAt` reads the last painted
        // frame's OSC 8 sequences): one scan serves every pane.
        self.frame_links = crate::hyperlinks::frame_link_ranges(&frame);
        frame
    }

    fn render_frame_inner(&mut self, width: usize, height: usize) -> Vec<Line> {
        // The click surface records this frame's geometry as the compose
        // computes it.
        self.click.clear();
        // An open overlay replaces the editor part of the dock (TS `showConfigurationMenu` /
        // `showSelector`); the prompt context stays above it. The onboarding splash covers the
        // whole pane instead (TS `showOverlay` 100%).
        let full_page = matches!(
            self.overlay,
            Some(Overlay::Captain(_) | Overlay::Usage(_) | Overlay::Router(_))
        ) || (matches!(self.overlay, Some(Overlay::ModelPicker(_)))
            && matches!(self.modal_backdrop.as_deref(), Some(Overlay::Captain(_))));
        let mut dock = if full_page {
            Vec::new()
        } else {
            render_prompt_context(&self.detail_label(), &self.theme, width)
        };
        // The info panel's CURRENT row budget (a resize re-budgets every frame).
        let info_viewport_rows = crate::session_ui::picker_viewport_rows(self.terminal_rows());
        let pane_row = dock.len();
        let speed_rows = usize::from(self.chrome.speed_text.is_some());
        let kb = self.editor.keybindings();
        let overlay_dock: Option<Vec<Line>> = match self.overlay.as_mut() {
            None => None,
            Some(Overlay::Onboarding(screen)) => {
                // Its rows select, so the frame-selection regions span the whole frame.
                let mut frame = screen.render(&self.theme, width, height, kb);
                self.frame_rows = frame.len();
                self.apply_frame_selection(&mut frame, 0, width);
                return frame;
            }
            Some(Overlay::ModelPicker(picker)) if self.modal_backdrop.is_some() => {
                // A machine-editor or Captain pick floats over its page instead of the chat.
                let page_rows = if full_page {
                    height.saturating_sub(1 + speed_rows)
                } else {
                    height.saturating_sub(FULLSCREEN_MIN_TRANSCRIPT_ROWS + 1 + pane_row)
                };
                let backdrop = match self.modal_backdrop.as_deref_mut() {
                    Some(Overlay::Captain(panel)) => {
                        panel.set_viewport_rows(page_rows);
                        panel.render(&self.theme, width, kb)
                    }
                    Some(Overlay::Workspace(page)) => {
                        page.set_viewport_rows(page_rows);
                        page.render(&self.theme, width, kb)
                    }
                    _ => Vec::new(),
                };
                let inset = if width >= 48 { 4 } else { 0 };
                let card_width = width.saturating_sub(2 * inset + 2);
                let card = picker.render(&self.theme, card_width, kb);
                let (rows, card_top) =
                    float_card(&self.theme, backdrop, card, width, inset, page_rows);
                self.click.record_picker(PickerClickSurface {
                    dock_row: pane_row + card_top + 1,
                    chrome_rows: MODEL_PICKER_CHROME_ROWS
                        + usize::from(picker.header().is_some())
                        + usize::from(picker.has_scoped_models()),
                    items: picker.filtered_window(),
                    kind: PickerKind::Model,
                });
                dock.extend(rows);
                Some(dock)
            }
            Some(Overlay::ModelPicker(picker)) => {
                dock.extend(picker.render(&self.theme, width, kb));
                // The pane's item rows are clickable: the recorded span
                // covers the filtered window the render drew.
                self.click.record_picker(PickerClickSurface {
                    dock_row: pane_row,
                    chrome_rows: MODEL_PICKER_CHROME_ROWS,
                    items: picker.filtered_window(),
                    kind: PickerKind::Model,
                });
                Some(dock)
            }
            Some(Overlay::ChoicePicker(picker)) => {
                dock.extend(picker.render(&self.theme, width, kb));
                self.click.record_picker(PickerClickSurface {
                    dock_row: pane_row,
                    chrome_rows: CHOICE_PICKER_CHROME_ROWS,
                    items: picker.visible_window(),
                    kind: PickerKind::Choice,
                });
                Some(dock)
            }
            Some(Overlay::Mcp(mcp_view)) => {
                dock.extend(mcp_view.render(&self.theme, width, kb));
                Some(dock)
            }
            Some(Overlay::Factory(factory_view)) => {
                dock.extend(factory_view.render(&self.theme, width, kb));
                Some(dock)
            }
            Some(Overlay::Library(library_view)) => {
                // Front cropping may remove the prompt context, but must leave the library
                // window intact above the speed footer.
                let library_rows = height
                    .saturating_sub(FULLSCREEN_MIN_TRANSCRIPT_ROWS)
                    .saturating_sub(usize::from(self.chrome.speed_text.is_some()))
                    .min(info_viewport_rows);
                dock.extend(library_view.render(&self.theme, width, kb, library_rows));
                Some(dock)
            }
            Some(Overlay::Captain(panel)) => {
                // The Captain panel fills the entire frame below the top bar.
                panel.set_viewport_rows(height.saturating_sub(1 + speed_rows));
                dock.extend(panel.render(&self.theme, width, kb));
                Some(dock)
            }
            Some(Overlay::Workspace(page)) => {
                page.set_viewport_rows(
                    height.saturating_sub(FULLSCREEN_MIN_TRANSCRIPT_ROWS + 1 + pane_row),
                );
                dock.extend(page.render(&self.theme, width, kb));
                Some(dock)
            }
            Some(Overlay::CrewPicker(picker)) => {
                dock.extend(picker.render(&self.theme, width, kb));
                Some(dock)
            }
            Some(Overlay::Heartbeats(picker)) => {
                dock.extend(picker.render(&self.theme, width, kb));
                Some(dock)
            }
            Some(Overlay::Goal(panel)) => {
                dock.extend(crate::goal_surface::render_goal_panel(
                    panel,
                    &self.theme,
                    width,
                    kb,
                ));
                Some(dock)
            }
            Some(Overlay::Bash(view)) => {
                dock.extend(view.render(&self.theme, width, kb));
                Some(dock)
            }
            Some(Overlay::Info(panel)) => {
                dock.extend(panel.render(
                    &self.theme,
                    width,
                    kb,
                    &self.code_block_indent,
                    info_viewport_rows,
                ));
                Some(dock)
            }
            Some(Overlay::Usage(panel)) => {
                let usage_rows = height.saturating_sub(1 + speed_rows);
                dock.extend(panel.render(&self.theme, width, kb, usage_rows));
                Some(dock)
            }
            Some(Overlay::Router(panel)) => {
                let router_rows = height.saturating_sub(1 + speed_rows);
                dock.extend(panel.render(&self.theme, width, kb, router_rows));
                Some(dock)
            }
            Some(Overlay::Tree(selector)) => {
                dock.extend(selector.render(&self.theme, width, kb));
                Some(dock)
            }
            Some(Overlay::Fork(selector)) => {
                dock.extend(selector.render(&self.theme, width, kb));
                Some(dock)
            }
            Some(Overlay::ShareLoader(loader)) => {
                let loader = loader.clone();
                dock.extend(self.render_share_loader(&loader, width));
                Some(dock)
            }
            Some(Overlay::Confirm { panel, .. }) => {
                dock.extend(panel.render(&self.theme, width, kb));
                Some(dock)
            }
            Some(Overlay::ProviderAuth(selector)) => {
                dock.extend(selector.render(&self.theme, width, kb));
                Some(dock)
            }
            Some(Overlay::AuthPanel(panel)) => {
                dock.extend(panel.render(&self.theme, width, kb));
                Some(dock)
            }
            Some(Overlay::Reload(message)) => {
                let message = message.clone();
                dock.extend(self.render_reload_box(&message, width));
                Some(dock)
            }
            Some(Overlay::Settings(menu)) => {
                dock.extend(menu.render(&self.theme, width, kb));
                Some(dock)
            }
        };
        // The top bar always renders: the surface is fullscreen-only
        // (operator ruling 2026-09-28 — the bar has no gate left).
        let top = render_top_bar(&self.chrome, &self.theme, width);
        let top_rows = 1;
        let dock = match overlay_dock {
            // The replacement surfaces swap only the editor part; the `/speed` footer stays
            // the dock's last row (TS `footerSlot`).
            Some(mut dock) => {
                if let Some(speed) = &self.chrome.speed_text {
                    dock.push(crate::chrome::render_speed_footer(
                        speed,
                        &self.theme,
                        width,
                    ));
                }
                dock
            }
            None => self.render_dock(width),
        };
        let max_dock = if full_page {
            height.saturating_sub(top_rows)
        } else {
            height.saturating_sub(FULLSCREEN_MIN_TRANSCRIPT_ROWS)
        };
        let dock_height = dock.len().min(max_dock);
        let cropped = dock.len().saturating_sub(dock_height);
        // The hardware cursor rides the dock's rows: a front crop removes
        // the first `cropped` rows, so subtract — or the reported cursor
        // lands below the editor.
        self.dock_cursor = self
            .dock_cursor
            .map(|(row, col)| (row.saturating_sub(cropped), col));
        let dock: Vec<Line> = if dock.len() > dock_height {
            dock[dock.len() - dock_height..].to_vec()
        } else {
            dock
        };
        let window_height = if full_page {
            0
        } else {
            height.saturating_sub(top_rows + dock.len()).max(
                FULLSCREEN_MIN_TRANSCRIPT_ROWS.min(height.saturating_sub(top_rows + dock.len())),
            )
        };
        let (window_rows, start) = if window_height == 0 {
            (Vec::new(), 0)
        } else {
            self.visible_transcript_window(width, window_height)
        };
        self.window_rows = window_height;
        // The selection restyle diff: only the rows the selection change
        // touched re-style.
        let window_rows = self.selection_styled_window(window_rows, start);
        let mut frame: Vec<Line> = Vec::with_capacity(height);
        frame.push(pad_row(top, width));
        for line in window_rows {
            frame.push(pad_row(line, width));
        }
        while frame.len() < height.saturating_sub(dock.len()) {
            frame.push(vec![Span::raw(" ".repeat(width))]);
        }
        // The click surface's frame scalars: a click's dock row indexes
        // the un-cropped dock.
        self.click.note_frame(top_rows, frame.len(), cropped);
        for line in dock {
            frame.push(pad_row(line, width));
        }
        // The hover affordance (operator directive 2026-09-26): the hovered card row brightens,
        // revalidated against THIS frame's click surface.
        if let Some((row, col)) = self.hover_pos {
            match self.click_target_at(row, col) {
                Some(click::ClickAction::ToggleCardExpansion(_)) => {
                    if let Some(line) = frame.get_mut(row) {
                        apply_hover_affordance(line, &self.theme);
                    }
                }
                // The dock's hover affordance (operator directive 2026-09-29): the hovered
                // segment or tray hint carries the ONE light band; the focused group's
                // selection band stays.
                Some(click::ClickAction::OpenDockGroup(_) | click::ClickAction::OpenAgentsView) => {
                    if let Some(region) = self.dock_region_at(row, col) {
                        if let Some(line) = frame.get_mut(row) {
                            self.theme.paint_hover_band(line, region.cols.clone());
                        }
                    }
                }
                _ => self.hover_pos = None,
            }
        }
        // A paused viewport carries the follow hint over the last window row — only when
        // following would actually scroll (operator directive 2026-09-26), and never on top
        // of full-page overlays where the transcript window is collapsed (`window_height == 0`).
        if window_height > 0 && !self.following && !self.window_shows_tail {
            if let Some(row) = frame.get_mut(window_height) {
                let key = self
                    .editor
                    .keybindings()
                    .first_key("tui.viewport.follow")
                    .unwrap_or_else(|| "ctrl+shift+down".to_string());
                let label = format!(" {key} to follow ");
                *row = composite_follow_hint(row, &label, width);
                // The hint's row never reads as the content beneath it.
                self.click.mask_rows(window_height, window_height + 1);
            }
        }
        self.frame_rows = frame.len();
        self.apply_frame_selection(
            &mut frame,
            crate::selection::HEADER_ROWS + self.window_rows,
            width,
        );
        // The action toasts overlay the transcript window's top rows. The overlay sits out an
        // in-progress selection drag: releasing over covered text could copy content that was
        // not visible.
        let now = std::time::Instant::now();
        let toasts: Vec<String> = if self.selection.is_dragging() {
            Vec::new()
        } else {
            self.toasts.active(now)
        };
        if !toasts.is_empty() {
            crate::toast::overlay_themed_toasts(
                &mut frame,
                top_rows,
                top_rows + window_height,
                &toasts,
                width,
                &self.theme,
            );
            // A click on the transient pill must not fire the hidden
            // row's target.
            let covered = toasts.len().min(window_height);
            self.click.mask_rows(top_rows, top_rows + covered);
        }
        frame
    }

    /// Hardware cursor position within the last composed frame, when the
    /// editor surface drew the cursor.
    pub fn frame_cursor(&self) -> Option<(usize, usize)> {
        // Every overlay but the `/mcp` view hides the editor's cursor.
        if self
            .overlay
            .as_ref()
            .is_some_and(|overlay| !matches!(overlay, Overlay::Mcp(_)))
        {
            return None;
        }
        self.dock_cursor
            .map(|(row, col)| (row + 1 + self.window_rows, col))
    }

    /// The inline layout the exit flush paints onto the main screen: the full transcript plus
    /// the dock, without the fullscreen window. These rows persist in the terminal's native
    /// scrollback.
    pub fn render_inline_frame(&mut self, width: usize) -> Vec<Line> {
        let mut rows = self.render_transcript(width);
        rows.extend(self.render_dock(width));
        // The inline layout has no fullscreen window on screen: a click
        // must never resolve against a dock the terminal does not show.
        self.click.clear();
        rows
    }
}

/// The hover affordance's row restyle (operator directive 2026-09-26): Muted spans brighten to
/// the theme's foreground, Dim to Muted.
fn apply_hover_affordance(row: &mut Line, theme: &crate::theme::Theme) {
    let muted = theme.fg_style(crate::theme::ThemeColor::Muted).fg;
    let dim = theme.fg_style(crate::theme::ThemeColor::Dim).fg;
    let text = theme.fg_style(crate::theme::ThemeColor::Text);
    let bright = theme.fg_style(crate::theme::ThemeColor::Muted);
    for span in row.iter_mut() {
        if span.style.fg == muted {
            span.style = span.style.patch(text);
        } else if span.style.fg == dim {
            span.style = span.style.patch(bright);
        }
    }
}

pub(super) fn indicator_row(indicator: &str, bg: Style, border: Style, width: usize) -> Line {
    // The indicator text paints on the editor surface's background too (operator directive
    // 2026-09-26): the `↑/↓ N more` rows read as part of the prompt bar.
    let mut row: Line = vec![Span::styled(indicator.to_string(), border.patch(bg))];
    let used = str_width(indicator);
    row.push(Span::styled(" ".repeat(width.saturating_sub(used)), bg));
    row
}

/// Pad a rendered row to the full width (default background).
pub(super) fn pad_row(line: Line, width: usize) -> Line {
    let used: usize = line.iter().map(|s| str_width(&s.content)).sum();
    let mut out = line;
    if used < width {
        out.push(Span::raw(" ".repeat(width - used)));
    }
    out
}

/// Composite the follow hint over one frame row (TS `renderFullscreen`: reversed, centered).
/// Leading OSC-133 zone markers stay at the row head so the marker plan keeps flagging.
pub(super) fn composite_follow_hint(row: &Line, label: &str, width: usize) -> Line {
    let label_width = str_width(label);
    let (markers, rest) = crate::osc133::split_leading_markers(row);
    let col = width.saturating_sub(label_width) / 2;
    let mut out: Line = markers;
    out.extend(crate::width::slice_line_by_column_strict(
        &rest, 0, col, true,
    ));
    out.push(Span::styled(
        label.to_string(),
        Style::default().add_modifier(Modifier::REVERSED),
    ));
    out.extend(crate::width::slice_line_by_column_strict(
        &rest,
        col.saturating_add(label_width),
        width,
        true,
    ));
    out
}

/// Float a framed card over a page: the page's rows dim, and the card replaces the middle rows
/// between `inset` columns of the page on each side. Returns the rows and the card's top row.
pub(super) fn float_card(
    theme: &Theme,
    page: Vec<Line>,
    card: Vec<Line>,
    width: usize,
    inset: usize,
    page_rows: usize,
) -> (Vec<Line>, usize) {
    let card_width = width.saturating_sub(2 * inset + 2);
    let card_height = card.len() + 2;
    let total = page.len().max(page_rows).max(card_height);
    let top = (total - card_height) / 2;
    let mut rows: Vec<Line> = page
        .into_iter()
        .map(|line| {
            line.into_iter()
                .map(|mut span| {
                    span.style = span.style.add_modifier(Modifier::DIM);
                    span
                })
                .collect()
        })
        .collect();
    rows.resize_with(total, Vec::new);
    let border = |text: String| theme.fg_span(ThemeColor::BorderAccent, text);
    let mut framed: Vec<Line> = Vec::with_capacity(card_height);
    framed.push(vec![border(format!(
        "\u{256d}{}\u{256e}",
        "\u{2500}".repeat(card_width)
    ))]);
    for line in card {
        let mut row = vec![border("\u{2502}".to_owned())];
        row.extend(pad_row(
            crate::width::truncate_line(&line, card_width, "\u{2026}"),
            card_width,
        ));
        row.push(border("\u{2502}".to_owned()));
        framed.push(row);
    }
    framed.push(vec![border(format!(
        "\u{2570}{}\u{256f}",
        "\u{2500}".repeat(card_width)
    ))]);
    for (offset, card_row) in framed.into_iter().enumerate() {
        let page_row = &rows[top + offset];
        let mut out = pad_row(
            crate::width::slice_line_by_column_strict(page_row, 0, inset, true),
            inset,
        );
        out.extend(card_row);
        out.extend(crate::width::slice_line_by_column_strict(
            page_row,
            width.saturating_sub(inset),
            inset,
            true,
        ));
        rows[top + offset] = out;
    }
    (rows, top)
}
