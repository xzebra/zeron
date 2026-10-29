//! Settings → Providers → Import history: per source (Codex, Claude Code),
//! how many sessions this device holds and how many are already here, with
//! "Import all" and "Choose…" handing off to the shell's import flow.

use gpui::{Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px};
use zeron_engine::history_import::HistorySourceStatus;
use zeron_rpc::methods;

use crate::popover::Loadable;
use crate::settings::widgets::{self, ActionTone};
use crate::shell::ImportHistory;
use crate::state::AppState;
use crate::theme::Theme;

pub struct HistoryImportCard {
    state: Entity<AppState>,
    sources: Loadable<Vec<HistorySourceStatus>>,
    /// Chat count at the last load: an import (or a delete) changes it, and
    /// the counts follow.
    loaded_chats: Option<usize>,
    task: Option<Task<()>>,
    _state: Subscription,
}

impl HistoryImportCard {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let state_sub = cx.observe(&state, |this: &mut Self, state, cx| {
            let chats = state.read(cx).chats.len();
            if this.task.is_none() && this.loaded_chats != Some(chats) {
                this.load(cx);
            }
        });
        let mut card = Self {
            state,
            sources: Loadable::Idle,
            loaded_chats: None,
            task: None,
            _state: state_sub,
        };
        card.load(cx);
        card
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.loaded_chats = Some(self.state.read(cx).chats.len());
        if !matches!(self.sources, Loadable::Ready(_)) {
            self.sources = Loadable::Loading;
        }
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_HISTORY_SOURCES, serde_json::json!({}))
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| {
                    serde_json::from_value::<Vec<HistorySourceStatus>>(v["sources"].clone())
                        .map_err(|e| e.to_string())
                });
            this.update(cx, |card, cx| {
                card.task = None;
                card.sources = match result {
                    Ok(sources) => Loadable::Ready(sources),
                    Err(error) => Loadable::Error(error),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn row(
        &self,
        source: &HistorySourceStatus,
        first: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let remaining = source.remaining();
        let meta: Vec<SharedString> = if source.total == 0 {
            vec![if source.available {
                "No sessions found on this device".into()
            } else {
                "Not used on this device".into()
            }]
        } else {
            let mut meta = vec![SharedString::from(sessions_label(source.total))];
            if source.imported > 0 {
                meta.push(format!("{} already here", source.imported).into());
            }
            meta
        };
        let id = source.id.clone();
        let choose = widgets::text_action(theme, ActionTone::Quiet, "Choose…")
            .id(SharedString::from(format!("history-choose-{}", source.id)))
            .on_click(cx.listener({
                let id = id.clone();
                move |_, _, window: &mut Window, cx| {
                    window.dispatch_action(
                        Box::new(ImportHistory {
                            source: Some(id.clone()),
                            all: false,
                        }),
                        cx,
                    );
                }
            }));
        let import_all = widgets::text_action(
            theme,
            ActionTone::Filled,
            if remaining > 0 {
                format!("Import all ({remaining})")
            } else {
                "All imported".to_string()
            },
        )
        .id(SharedString::from(format!("history-all-{}", source.id)))
        .when(remaining == 0, |button| {
            button.opacity(0.5).cursor_default()
        })
        .when(remaining > 0, |button| {
            button.on_click(cx.listener(move |_, _, window: &mut Window, cx| {
                window.dispatch_action(
                    Box::new(ImportHistory {
                        source: Some(id.clone()),
                        all: true,
                    }),
                    cx,
                );
            }))
        });
        widgets::card_row(theme, first)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(widgets::row_title(theme, source.label.clone()))
                    .child(widgets::meta_line(
                        theme,
                        meta.into_iter().map(|m| m.into_any_element()).collect(),
                    )),
            )
            .when(source.total > 0, |row| {
                row.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(choose)
                        .child(import_all),
                )
            })
            .into_any_element()
    }
}

fn sessions_label(count: usize) -> String {
    if count == 1 {
        "1 session".into()
    } else {
        format!("{count} sessions")
    }
}

impl Render for HistoryImportCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).for_settings_surface();
        let block =
            match &self.sources {
                Loadable::Ready(sources) => {
                    let sources = sources.clone();
                    let rows: Vec<_> = sources
                        .iter()
                        .enumerate()
                        .map(|(ix, source)| self.row(source, ix == 0, &theme, cx))
                        .collect();
                    widgets::section_card(&theme).mt_0().children(rows)
                }
                Loadable::Error(message) => widgets::section_card(&theme).mt_0().child(
                    widgets::card_row(&theme, true).child(widgets::meta_line(
                        &theme,
                        vec![
                            SharedString::from(format!("Could not read history: {message}"))
                                .into_any_element(),
                        ],
                    )),
                ),
                Loadable::Idle | Loadable::Loading => widgets::section_card(&theme).mt_0().child(
                    widgets::card_row(&theme, true).child(widgets::meta_line(
                        &theme,
                        vec![SharedString::from("Looking for sessions…").into_any_element()],
                    )),
                ),
            };
        div()
            .child(widgets::section(&theme, "Import history", block))
            .child(widgets::page_subtitle(
                &theme,
                "Bring conversations from the Codex and Claude Code CLIs on this device into \
                 Zeron. Continuing an imported chat resumes the original session.",
            ))
    }
}
