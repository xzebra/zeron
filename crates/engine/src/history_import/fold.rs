//! Replayed transcript events folded into chat-doc entries of the same shape
//! a live run writes: one entry per prompt, one per reply, the reply's text,
//! reasoning and tool parts produced by the live parts fold and passed
//! through the same render-only privacy policy (tool inputs sanitized,
//! outputs never stored).

use zeron_doc::{
    MessagePart, MessageRole, MessageStatus, SessionMessageEntry, fold_event_into_parts,
    sanitize_tool_call,
};
use zeron_harness::history::TranscriptEvent;
use zeron_proto::AgentEvent;

pub(crate) struct EntryFold {
    device_id: String,
    entries: Vec<SessionMessageEntry>,
    parts: Vec<MessagePart>,
    /// First and last event times of the reply being folded.
    reply_span: Option<(i64, i64)>,
}

impl EntryFold {
    pub(crate) fn new(device_id: &str) -> Self {
        Self {
            device_id: device_id.to_string(),
            entries: Vec::new(),
            parts: Vec::new(),
            reply_span: None,
        }
    }

    pub(crate) fn push(&mut self, item: TranscriptEvent) {
        let TranscriptEvent { at_ms, event } = item;
        match event {
            AgentEvent::UserMessage { text } => {
                self.finish_reply();
                self.entries.push(SessionMessageEntry {
                    id: crate::new_id(),
                    role: MessageRole::User,
                    parts: vec![MessagePart::Text {
                        id: "t0".into(),
                        text,
                    }],
                    created_at: at_ms,
                    device_id: self.device_id.clone(),
                    status: Some(MessageStatus::Complete),
                    continuation_of: None,
                    duration_ms: None,
                });
            }
            AgentEvent::ToolCall { id, call } => self.fold(
                at_ms,
                AgentEvent::ToolCall {
                    id,
                    call: sanitize_tool_call(&call),
                },
            ),
            AgentEvent::ToolResult { id, is_error, .. } => {
                // Resolves a chip in place; never opens a reply by itself.
                if !self.parts.is_empty() {
                    fold_event_into_parts(
                        &mut self.parts,
                        &AgentEvent::ToolResult {
                            id,
                            is_error,
                            output: None,
                            diff: None,
                        },
                    );
                }
            }
            AgentEvent::TextDelta { text } => {
                // Each replayed delta is a whole block: keep blocks apart the
                // way a streamed reply's own newlines would.
                if matches!(self.parts.last(), Some(MessagePart::Text { .. })) {
                    self.fold(
                        at_ms,
                        AgentEvent::TextDelta {
                            text: "\n\n".into(),
                        },
                    );
                }
                self.fold(at_ms, AgentEvent::TextDelta { text });
            }
            AgentEvent::ReasoningDelta { text } => {
                if matches!(self.parts.last(), Some(MessagePart::Reasoning { .. })) {
                    self.fold(
                        at_ms,
                        AgentEvent::ReasoningDelta {
                            text: "\n\n".into(),
                        },
                    );
                }
                self.fold(at_ms, AgentEvent::ReasoningDelta { text });
            }
            AgentEvent::Error { .. } | AgentEvent::Done { .. } => {}
            other => self.fold(at_ms, other),
        }
    }

    fn fold(&mut self, at_ms: i64, event: AgentEvent) {
        fold_event_into_parts(&mut self.parts, &event);
        self.reply_span = Some(match self.reply_span {
            Some((first, last)) => (first, last.max(at_ms)),
            None => (at_ms, at_ms),
        });
    }

    fn finish_reply(&mut self) {
        let (first, last) = self.reply_span.take().unwrap_or_default();
        if self.parts.is_empty() {
            return;
        }
        self.entries.push(SessionMessageEntry {
            id: crate::new_id(),
            role: MessageRole::Assistant,
            parts: std::mem::take(&mut self.parts),
            created_at: first,
            device_id: self.device_id.clone(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            duration_ms: (last > first).then_some(last - first),
        });
    }

    pub(crate) fn finish(mut self) -> Vec<SessionMessageEntry> {
        self.finish_reply();
        self.entries
    }
}

/// The sidebar preview of a transcript: its last text, as live runs set it.
pub(crate) fn last_text(entries: &[SessionMessageEntry]) -> Option<String> {
    entries.iter().rev().find_map(|entry| {
        entry.parts.iter().rev().find_map(|part| match part {
            MessagePart::Text { text, .. } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::ToolCall;

    fn at(at_ms: i64, event: AgentEvent) -> TranscriptEvent {
        TranscriptEvent { at_ms, event }
    }

    #[test]
    fn a_turn_becomes_a_prompt_then_one_reply() {
        let mut fold = EntryFold::new("dev");
        for item in [
            at(
                1_000,
                AgentEvent::UserMessage {
                    text: "fix it".into(),
                },
            ),
            at(
                2_000,
                AgentEvent::ReasoningDelta {
                    text: "thinking".into(),
                },
            ),
            at(
                2_100,
                AgentEvent::TextDelta {
                    text: "On it.".into(),
                },
            ),
            at(
                2_200,
                AgentEvent::ToolCall {
                    id: "t1".into(),
                    call: ToolCall::WriteFile {
                        path: "a.rs".into(),
                        content: Some("secret body".into()),
                    },
                },
            ),
            at(
                2_300,
                AgentEvent::ToolResult {
                    id: "t1".into(),
                    is_error: true,
                    output: Some("boom".into()),
                    diff: None,
                },
            ),
            at(
                2_400,
                AgentEvent::TextDelta {
                    text: "Done.".into(),
                },
            ),
            at(
                2_500,
                AgentEvent::TextDelta {
                    text: "Really.".into(),
                },
            ),
        ] {
            fold.push(item);
        }
        let entries = fold.finish();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role, MessageRole::User);
        assert_eq!(entries[1].role, MessageRole::Assistant);
        assert_eq!(entries[1].created_at, 2_000);
        assert_eq!(entries[1].duration_ms, Some(500));
        let parts = &entries[1].parts;
        assert!(matches!(&parts[0], MessagePart::Reasoning { text, .. } if text == "thinking"));
        assert!(matches!(&parts[1], MessagePart::Text { text, .. } if text == "On it."));
        assert!(matches!(
            &parts[2],
            MessagePart::Tool {
                call: ToolCall::WriteFile { content: None, .. },
                resolved: true,
                is_error: true,
                output: None,
                ..
            }
        ));
        assert!(matches!(&parts[3], MessagePart::Text { text, .. } if text == "Done.\n\nReally."));
        assert_eq!(last_text(&entries).as_deref(), Some("Done.\n\nReally."));
    }

    #[test]
    fn a_reply_without_a_prompt_still_lands() {
        let mut fold = EntryFold::new("dev");
        fold.push(at(
            5,
            AgentEvent::ToolResult {
                id: "x".into(),
                is_error: false,
                output: None,
                diff: None,
            },
        ));
        fold.push(at(
            10,
            AgentEvent::TextDelta {
                text: "resumed".into(),
            },
        ));
        let entries = fold.finish();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role, MessageRole::Assistant);
    }
}
