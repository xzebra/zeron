//! Import sessions another agent CLI recorded on this device (Codex, Claude
//! Code) as chats: a picker listing a source's sessions newest first, with a
//! bulk row for everything not imported yet, and the progress dialog the
//! settings page's "Import all" opens directly.
use super::command_palette::{
    command_key_hint, palette_card, palette_empty, palette_footer, palette_header, palette_overlay,
    palette_results_fade, palette_results_height,
};
use super::*;
use zeron_engine::history_import::{
    HistoryImportEvent, HistorySessionList, HistorySessionRow, HistorySessionState,
    HistorySourceStatus,
};

/// Open the history import: the picker for `source` (the first source with
/// sessions left when `None`), or straight into importing all of it.
#[derive(Clone, PartialEq, Action)]
#[action(namespace = shell, no_json)]
pub struct ImportHistory {
    pub source: Option<String>,
    pub all: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Row {
    /// Import every session not imported yet.
    All(usize),
    /// Index into the filtered session list.
    Session(usize),
}

#[derive(Clone, Debug)]
enum Progress {
    Running {
        done: usize,
        total: usize,
        current: Option<SharedString>,
    },
    Done {
        imported: usize,
        failed: usize,
        errors: Vec<String>,
        chat_id: Option<String>,
    },
    Failed(SharedString),
}

pub(super) struct HistoryImportFlow {
    search: Entity<ComposerInput>,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    focus_pending: bool,
    sources: Loadable<Vec<HistorySourceStatus>>,
    /// The source the picker lists; chosen once sources load.
    source: Option<String>,
    /// Import everything left as soon as the source is known.
    import_all: bool,
    /// Opened straight into an import (Settings' "Import all"): finishing
    /// closes rather than returning to a picker nobody opened.
    close_when_done: bool,
    sessions: Loadable<HistorySessionList>,
    active: usize,
    scroll: gpui::ScrollHandle,
    progress: Option<Progress>,
    sources_task: Option<Task<()>>,
    sessions_task: Option<Task<()>>,
    import_task: Option<Task<()>>,
    _search_events: Subscription,
}

fn matches_query(query: &str, row: &HistorySessionRow) -> bool {
    let text = format!(
        "{} {}",
        row.title.as_deref().unwrap_or(""),
        row.cwd.as_deref().unwrap_or("")
    )
    .to_lowercase();
    query.split_whitespace().all(|word| text.contains(word))
}

fn state_label(row: &HistorySessionRow) -> Option<&'static str> {
    match row.state {
        HistorySessionState::New => None,
        HistorySessionState::Imported => Some("Imported"),
        HistorySessionState::Native => Some("In Zeron"),
        HistorySessionState::Removed => Some("Deleted"),
    }
}

/// The folder a session ran in, shortened to its last two segments.
fn short_folder(cwd: &str) -> String {
    let parts: Vec<&str> = cwd.trim_end_matches('/').rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

impl Shell {
    pub(super) fn open_history_import(
        &mut self,
        action: &ImportHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .history_import
            .as_ref()
            .is_some_and(|flow| flow.import_task.is_some())
        {
            return;
        }
        self.command_palette = None;
        self.close_add_space(cx);
        let search =
            cx.new(|cx| ComposerInput::with_context("Search sessions…", "PaletteSearch", cx));
        let events = cx.subscribe(&search, |this, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited)
                && let Some(flow) = this.history_import.as_mut()
            {
                flow.active = 0;
                flow.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
                cx.notify();
            }
        });
        let previous_focus = self
            .history_import
            .take()
            .and_then(|flow| flow.previous_focus)
            .or_else(|| window.focused(cx));
        self.history_import = Some(HistoryImportFlow {
            search,
            focus: cx.focus_handle(),
            previous_focus,
            focus_pending: true,
            sources: Loadable::Loading,
            source: action.source.clone(),
            import_all: action.all,
            close_when_done: action.all,
            sessions: Loadable::Idle,
            active: 0,
            scroll: gpui::ScrollHandle::new(),
            progress: action.all.then_some(Progress::Running {
                done: 0,
                total: 0,
                current: None,
            }),
            sources_task: None,
            sessions_task: None,
            import_task: None,
            _search_events: events,
        });
        self.load_history_sources(cx);
        cx.notify();
    }

    pub(super) fn close_history_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(flow) = self.history_import.take() {
            if let Some(focus) = flow.previous_focus {
                window.focus(&focus, cx);
            }
            cx.notify();
        }
    }

    fn load_history_sources(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            if let Some(flow) = self.history_import.as_mut() {
                flow.sources = Loadable::Error("Engine not connected".into());
            }
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_HISTORY_SOURCES, serde_json::json!({}))
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| {
                    serde_json::from_value::<Vec<HistorySourceStatus>>(v["sources"].clone())
                        .map_err(|e| e.to_string())
                });
            this.update(cx, |shell, cx| {
                let Some(flow) = shell.history_import.as_mut() else {
                    return;
                };
                flow.sources_task = None;
                match result {
                    Ok(sources) => {
                        if flow.source.is_none() {
                            flow.source = sources
                                .iter()
                                .find(|s| s.remaining() > 0)
                                .or_else(|| sources.iter().find(|s| s.total > 0))
                                .or(sources.first())
                                .map(|s| s.id.clone());
                        }
                        flow.sources = Loadable::Ready(sources);
                        if std::mem::take(&mut flow.import_all) {
                            shell.start_history_import(true, Vec::new(), cx);
                        } else {
                            shell.load_history_sessions(cx);
                        }
                    }
                    Err(error) => {
                        flow.sources = Loadable::Error(error.clone());
                        if flow.progress.is_some() {
                            flow.progress = Some(Progress::Failed(error.into()));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(flow) = self.history_import.as_mut() {
            flow.sources_task = Some(task);
        }
    }

    fn load_history_sessions(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(flow) = self.history_import.as_mut() else {
            return;
        };
        let Some(source) = flow.source.clone() else {
            flow.sessions = Loadable::Ready(HistorySessionList {
                sessions: Vec::new(),
                total: 0,
            });
            return;
        };
        flow.sessions = Loadable::Loading;
        flow.active = 0;
        flow.sessions_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::LIST_HISTORY_SESSIONS,
                    serde_json::json!({ "source": source }),
                )
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| {
                    serde_json::from_value::<HistorySessionList>(v).map_err(|e| e.to_string())
                });
            this.update(cx, |shell, cx| {
                if let Some(flow) = shell.history_import.as_mut()
                    && flow.source.as_deref() == Some(source.as_str())
                {
                    flow.sessions_task = None;
                    flow.sessions = match result {
                        Ok(list) => Loadable::Ready(list),
                        Err(error) => Loadable::Error(error),
                    };
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn select_history_source(&mut self, source: String, cx: &mut Context<Self>) {
        let Some(flow) = self.history_import.as_mut() else {
            return;
        };
        if flow.source.as_deref() == Some(source.as_str()) {
            return;
        }
        flow.source = Some(source);
        flow.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
        self.load_history_sessions(cx);
        cx.notify();
    }

    /// Sessions matching the query, newest first as the engine listed them.
    fn history_sessions(&self, cx: &App) -> Vec<HistorySessionRow> {
        let Some(flow) = &self.history_import else {
            return Vec::new();
        };
        let Some(list) = flow.sessions.ready() else {
            return Vec::new();
        };
        let query = flow.search.read(cx).text().trim().to_lowercase();
        list.sessions
            .iter()
            .filter(|row| matches_query(&query, row))
            .cloned()
            .collect()
    }

    fn history_source_remaining(&self) -> usize {
        let Some(flow) = &self.history_import else {
            return 0;
        };
        flow.sources
            .ready()
            .and_then(|sources| {
                sources
                    .iter()
                    .find(|s| Some(s.id.as_str()) == flow.source.as_deref())
            })
            .map_or(0, HistorySourceStatus::remaining)
    }

    fn history_rows(&self, cx: &App) -> Vec<Row> {
        let sessions = self.history_sessions(cx);
        let searching = self
            .history_import
            .as_ref()
            .is_some_and(|flow| !flow.search.read(cx).text().trim().is_empty());
        let remaining = self.history_source_remaining();
        let mut rows = Vec::new();
        if remaining > 1 && !searching {
            rows.push(Row::All(remaining));
        }
        rows.extend((0..sessions.len()).map(Row::Session));
        rows
    }

    fn activate_history_row(&mut self, row: Row, window: &mut Window, cx: &mut Context<Self>) {
        match row {
            Row::All(_) => self.start_history_import(true, Vec::new(), cx),
            Row::Session(ix) => {
                let Some(session) = self.history_sessions(cx).into_iter().nth(ix) else {
                    return;
                };
                match (session.state, session.chat_id) {
                    (HistorySessionState::Imported | HistorySessionState::Native, Some(chat)) => {
                        self.close_history_import(window, cx);
                        self.open_chat(chat, cx);
                    }
                    _ => self.start_history_import(false, vec![session.id], cx),
                }
            }
        }
    }

    /// Stream one import run, mirroring the profile importer's plumbing.
    fn start_history_import(
        &mut self,
        all: bool,
        session_ids: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let engine = self.state.read(cx).engine().cloned();
        let Some(flow) = self.history_import.as_mut() else {
            return;
        };
        if flow.import_task.is_some() {
            return;
        }
        let (Some(engine), Some(source)) = (engine, flow.source.clone()) else {
            flow.progress = Some(Progress::Failed("Nothing to import".into()));
            cx.notify();
            return;
        };
        flow.progress = Some(Progress::Running {
            done: 0,
            total: 0,
            current: None,
        });
        let params = serde_json::json!({
            "source": source,
            "sessionIds": session_ids,
            "all": all,
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
        let stream = Tokio::spawn(cx, async move {
            let mut items = engine
                .client()
                .subscribe(methods::IMPORT_HISTORY, params)
                .await
                .map_err(|error| error.to_string())?;
            while let Some(item) = items.recv().await {
                let _ = tx.send(item);
            }
            Ok::<(), String>(())
        });
        flow.import_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let item = rx.recv().await;
                let ended = item.is_none();
                this.update(cx, |shell, cx| {
                    if let Some(item) = item
                        && let Ok(event) = serde_json::from_value::<HistoryImportEvent>(item)
                    {
                        shell.apply_history_import_event(event);
                    }
                    if ended && let Some(flow) = shell.history_import.as_mut() {
                        flow.import_task = None;
                        // A stream that ended before its summary never finished.
                        if matches!(flow.progress, Some(Progress::Running { .. })) {
                            flow.progress = Some(Progress::Failed(
                                "The import stopped before it finished.".into(),
                            ));
                        }
                    }
                    cx.notify();
                })
                .ok();
                if ended {
                    break;
                }
            }
            if let Ok(Err(error)) = stream.await {
                this.update(cx, |shell, cx| {
                    if let Some(flow) = shell.history_import.as_mut() {
                        flow.progress = Some(Progress::Failed(error.into()));
                        cx.notify();
                    }
                })
                .ok();
            }
        }));
        cx.notify();
    }

    fn apply_history_import_event(&mut self, event: HistoryImportEvent) {
        let Some(flow) = self.history_import.as_mut() else {
            return;
        };
        flow.progress = Some(match event {
            HistoryImportEvent::Start { total } => Progress::Running {
                done: 0,
                total,
                current: None,
            },
            HistoryImportEvent::Session {
                index,
                total,
                title,
            } => Progress::Running {
                done: index,
                total,
                current: title.map(SharedString::from),
            },
            HistoryImportEvent::Summary {
                imported,
                failed,
                errors,
                chat_id,
                ..
            } => Progress::Done {
                imported,
                failed,
                errors,
                chat_id,
            },
        });
    }

    /// Leave the progress dialog: into the chat a single import wrote, else
    /// back to the refreshed picker.
    fn finish_history_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let opened = match self
            .history_import
            .as_ref()
            .and_then(|f| f.progress.clone())
        {
            Some(Progress::Done { chat_id, .. }) => chat_id,
            _ => None,
        };
        let close = self
            .history_import
            .as_ref()
            .is_some_and(|flow| flow.close_when_done);
        match opened {
            Some(chat_id) => {
                self.close_history_import(window, cx);
                self.open_chat(chat_id, cx);
            }
            None if close => self.close_history_import(window, cx),
            None => {
                if let Some(flow) = self.history_import.as_mut() {
                    flow.progress = None;
                    flow.focus_pending = true;
                }
                self.load_history_sources(cx);
                cx.notify();
            }
        }
    }

    pub(super) fn render_history_import_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).for_popup();
        if let Some(progress) = self.history_import.as_ref()?.progress.clone() {
            return Some(self.render_history_import_progress(progress, viewport, &theme, cx));
        }
        let rows = self.history_rows(cx);
        let sessions = self.history_sessions(cx);
        let flow = self.history_import.as_mut()?;
        if std::mem::take(&mut flow.focus_pending) {
            window.focus(&flow.search.focus_handle(cx), cx);
        }
        flow.active = flow.active.min(rows.len().saturating_sub(1));
        let active = flow.active;
        let search = flow.search.clone();
        let query = search.read(cx).text().to_string();
        let focus = flow.focus.clone();
        let scroll = flow.scroll.clone();
        let loading = matches!(flow.sessions, Loadable::Idle | Loadable::Loading)
            || matches!(flow.sources, Loadable::Loading);
        let error = match (&flow.sources, &flow.sessions) {
            (Loadable::Error(e), _) | (_, Loadable::Error(e)) => Some(e.clone()),
            _ => None,
        };
        let current = flow.source.clone();
        let sources = flow.sources.ready().cloned().unwrap_or_default();

        let mut list = Vec::new();
        let row_count = rows.len();
        for (ix, row) in rows.iter().enumerate() {
            let content = match row {
                Row::All(count) => popover::menu_row(&theme, ix == active, format!("hi-all-{ix}"))
                    .rounded(px(popover::PALETTE_ITEM_RADIUS))
                    .min_h(px(30.0))
                    .py(px(4.0))
                    .child(
                        icon(icons::CLOCK_CIRCLE)
                            .size(px(16.0))
                            .flex_none()
                            .text_color(theme.text_muted),
                    )
                    .child(SharedString::from(format!(
                        "Import all {count} sessions not imported yet"
                    )))
                    .into_any_element(),
                Row::Session(session_ix) => {
                    let Some(session) = sessions.get(*session_ix) else {
                        continue;
                    };
                    let title = session
                        .title
                        .clone()
                        .unwrap_or_else(|| "Untitled session".into());
                    let when = DateTime::from_timestamp_millis(session.updated_at)
                        .map(|at| format_time_ago(at, Utc::now()))
                        .unwrap_or_default();
                    let detail = match session.cwd.as_deref() {
                        Some(cwd) => format!("{} · {when}", short_folder(cwd)),
                        None => when,
                    };
                    popover::menu_row(&theme, ix == active, format!("hi-session-{ix}"))
                        .rounded(px(popover::PALETTE_ITEM_RADIUS))
                        .min_h(px(44.0))
                        .py(px(4.0))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .min_w_0()
                                .flex_1()
                                .child(div().truncate().child(popover::search_highlight(
                                    transcript::single_line(&title).into(),
                                    Some(&query),
                                    &theme,
                                )))
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(crate::typography::ui_rems(11.0))
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(detail)),
                                ),
                        )
                        .when_some(state_label(session), |row, label| {
                            row.child(
                                div()
                                    .flex_none()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text_muted)
                                    .child(label),
                            )
                        })
                        .into_any_element()
                }
            };
            let target = row.clone();
            list.push(
                div()
                    .id(("history-import-row", ix))
                    .flex_none()
                    .px(px(8.0))
                    .when(ix == 0, |row| row.pt(px(8.0)))
                    .when(ix + 1 == row_count, |row| row.pb(px(8.0)))
                    .on_mouse_move(cx.listener(move |this, _: &gpui::MouseMoveEvent, _, cx| {
                        if let Some(flow) = this.history_import.as_mut()
                            && flow.active != ix
                        {
                            flow.active = ix;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_history_row(target.clone(), window, cx)
                    }))
                    .child(content)
                    .into_any_element(),
            );
        }
        let empty = match (&error, loading) {
            (Some(error), _) => Some(("Could not read sessions", error.clone())),
            (None, true) => Some(("Looking for sessions…", String::new())),
            (None, false) if rows.is_empty() && query.trim().is_empty() => Some((
                "No sessions found",
                "Nothing from this CLI is recorded on this device.".into(),
            )),
            (None, false) if rows.is_empty() => {
                Some(("No matches", "Try a title or folder name.".into()))
            }
            _ => None,
        };
        let body = div()
            .id("history-import-results")
            .min_h_0()
            .max_h(px(palette_results_height(viewport)))
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .flex()
            .flex_col()
            .gap(px(SIDEBAR_LIST_GAP))
            .children(list)
            .when_some(empty, |el, (title, hint)| {
                el.child(palette_empty(&theme, title, hint))
            });
        let body = palette_results_fade(body, &scroll);
        let tabs =
            div()
                .flex_none()
                .px(px(12.0))
                .py(px(6.0))
                .flex()
                .items_center()
                .gap(px(4.0))
                .border_b_1()
                .border_color(crate::theme::hairline(0.06))
                .children(sources.into_iter().enumerate().map(|(ix, source)| {
                    let selected = current.as_deref() == Some(source.id.as_str());
                    let id = source.id.clone();
                    crate::surface_chrome::tab(("history-source", ix), selected, &theme)
                        .px(px(8.0))
                        .role(gpui::Role::Tab)
                        .aria_label(SharedString::from(source.label.clone()))
                        .gap(px(6.0))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_history_source(id.clone(), cx)
                        }))
                        .child(SharedString::from(source.label.clone()))
                        .child(
                            div()
                                .text_color(theme.text_muted)
                                .child(SharedString::from(source.total.to_string())),
                        )
                }));
        let card = palette_card("history-import", &focus, viewport, &theme)
            .on_key_down(
                cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                    match event.keystroke.key.as_str() {
                        "up" | "down" => {
                            let count = this.history_rows(cx).len();
                            if count > 0
                                && let Some(flow) = this.history_import.as_mut()
                            {
                                flow.active = if event.keystroke.key == "down" {
                                    (flow.active + 1) % count
                                } else {
                                    (flow.active + count - 1) % count
                                };
                                flow.scroll.scroll_to_item(flow.active);
                                cx.notify();
                            }
                        }
                        "enter" if !event.is_held => {
                            let rows = this.history_rows(cx);
                            if let Some(row) = this
                                .history_import
                                .as_ref()
                                .and_then(|f| rows.get(f.active))
                                .cloned()
                            {
                                this.activate_history_row(row, window, cx);
                            }
                        }
                        "escape" => this.close_history_import(window, cx),
                        _ => return,
                    }
                    cx.stop_propagation();
                }),
            )
            .on_mouse_down_out(
                cx.listener(|this, _, window, cx| this.close_history_import(window, cx)),
            )
            .child(palette_header(&theme, search.into_any_element(), div()))
            .child(tabs)
            .child(body)
            .child(
                palette_footer()
                    .child(command_key_hint(&theme, "↑ ↓", "Navigate"))
                    .child(command_key_hint(&theme, "↵", "Import"))
                    .child(command_key_hint(&theme, "Esc", "Close")),
            );
        Some(palette_overlay(viewport, card))
    }

    fn render_history_import_progress(
        &mut self,
        progress: Progress,
        viewport: gpui::Size<Pixels>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let card = match progress {
            Progress::Running {
                done,
                total,
                current,
            } => {
                let label = match (current, total) {
                    (Some(title), total) if total > 1 => {
                        format!("{title} ({} of {total})", done + 1)
                    }
                    (Some(title), _) => title.to_string(),
                    (None, _) => "Reading sessions…".to_string(),
                };
                popover::dialog_card(theme)
                    .child(popover::dialog_title(theme, "Importing"))
                    .child(popover::dialog_body(theme, label))
            }
            Progress::Done {
                imported,
                failed,
                errors,
                chat_id,
            } => {
                let mut body = match imported {
                    0 => "Nothing new to import.".to_string(),
                    1 => "Imported 1 session.".to_string(),
                    n => format!("Imported {n} sessions."),
                };
                if failed > 0 {
                    body.push_str(&format!(
                        " {failed} could not be read{}",
                        errors
                            .first()
                            .map(|e| format!(": {e}"))
                            .unwrap_or_else(|| ".".into())
                    ));
                }
                let label = if chat_id.is_some() { "Open" } else { "Done" };
                popover::dialog_card(theme)
                    .child(popover::dialog_title(theme, "Import finished"))
                    .child(popover::dialog_body(theme, body))
                    .child(
                        div().flex().justify_end().child(
                            popover::btn_primary(theme, label)
                                .id("history-import-finish")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.finish_history_import(window, cx)
                                })),
                        ),
                    )
            }
            Progress::Failed(error) => popover::dialog_card(theme)
                .child(popover::dialog_title(theme, "Import failed"))
                .child(popover::dialog_body(theme, error))
                .child(
                    div().flex().justify_end().child(
                        popover::btn_ghost(theme, "Close", "history-import-close")
                            .id("history-import-dismiss")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_history_import(window, cx)
                            })),
                    ),
                ),
        };
        popover::modal("history-import-dialog", viewport, card.into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(title: &str, cwd: &str) -> HistorySessionRow {
        HistorySessionRow {
            id: "s".into(),
            title: Some(title.into()),
            cwd: Some(cwd.into()),
            model: None,
            started_at: None,
            updated_at: 0,
            size_bytes: 0,
            state: HistorySessionState::New,
            chat_id: None,
        }
    }

    #[test]
    fn search_matches_title_and_folder() {
        let session = row("Fix the flaky test", "/Users/me/zeron");
        assert!(matches_query("flaky zeron", &session));
        assert!(matches_query("", &session));
        assert!(!matches_query("codex", &session));
    }

    #[test]
    fn folders_shorten_to_their_last_two_segments() {
        assert_eq!(short_folder("/Users/me/projects/zeron/"), "projects/zeron");
        assert_eq!(short_folder("zeron"), "zeron");
    }

    fn shell_window(
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::WindowHandle<Shell>, tempfile::TempDir) {
        use gpui::AppContext;
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zeron_proto::HarnessId::Mock,
                },
                cx,
            )
        });
        (window, dir)
    }

    #[gpui::test]
    fn the_picker_lists_sessions_under_a_bulk_row(cx: &mut gpui::TestAppContext) {
        let (window, _dir) = shell_window(cx);
        window
            .update(cx, |shell, window, cx| {
                shell.open_history_import(
                    &ImportHistory {
                        source: Some("claude".into()),
                        all: false,
                    },
                    window,
                    cx,
                );
                let flow = shell.history_import.as_mut().expect("flow open");
                flow.sources = Loadable::Ready(vec![HistorySourceStatus {
                    id: "claude".into(),
                    label: "Claude Code".into(),
                    available: true,
                    total: 3,
                    imported: 1,
                }]);
                let mut imported = row("Already here", "/w/a");
                imported.id = "s3".into();
                imported.state = HistorySessionState::Imported;
                imported.chat_id = Some("chat-3".into());
                flow.sessions = Loadable::Ready(HistorySessionList {
                    sessions: vec![
                        row("Fix the flaky test", "/w/zeron"),
                        row("Another one", "/w/zeron"),
                        imported,
                    ],
                    total: 3,
                });
                assert_eq!(
                    shell.history_rows(cx),
                    [
                        Row::All(2),
                        Row::Session(0),
                        Row::Session(1),
                        Row::Session(2)
                    ]
                );
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();
        window
            .update(cx, |shell, window, cx| {
                // An imported session opens its chat instead of importing again.
                shell.activate_history_row(Row::Session(2), window, cx);
                assert!(shell.history_import.is_none());
            })
            .unwrap();
    }
}
