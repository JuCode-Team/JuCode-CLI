use std::time::Instant;

use crate::terminal_renderer::TerminalRenderer;
use crate::ui_builder::UiBuilder;
use crate::{
    wrap_lines, BottomStatus, ChatLine, UiDocument, UiLine, CURSOR_MARKER, VISIBLE_CURSOR,
};

pub struct RenderFrameBench {
    history: Vec<ChatLine>,
    cached_history_lines: Vec<UiLine>,
    renderer: TerminalRenderer,
    tick: usize,
}

impl RenderFrameBench {
    pub fn new(history_items: usize, width: usize) -> Self {
        let history = build_history(history_items);
        let cached_history_lines = render_history_lines(&history, width);
        Self {
            history,
            cached_history_lines,
            renderer: TerminalRenderer::new(),
            tick: 0,
        }
    }

    pub fn render_cold_frame(&mut self, width: usize, height: u16) -> usize {
        self.tick = self.tick.wrapping_add(1);
        let history_lines = render_history_lines(&self.history, width);
        let document = build_document(history_lines, width, self.tick);
        let mut renderer = TerminalRenderer::new();
        renderer.render_document_for_bench(&document, width as u16, height)
    }

    pub fn render_cached_frame(&mut self, width: usize, height: u16) -> usize {
        self.tick = self.tick.wrapping_add(1);
        let document = build_document(self.cached_history_lines.clone(), width, self.tick);
        self.renderer
            .render_document_for_bench(&document, width as u16, height)
    }
}

fn build_history(history_items: usize) -> Vec<ChatLine> {
    (0..history_items)
        .map(|index| {
            ChatLine::Assistant(format!(
                "assistant line {index}: this is stable transcript text used to measure wrapping and rendering"
            ))
        })
        .collect()
}

fn render_history_lines(history: &[ChatLine], width: usize) -> Vec<UiLine> {
    let history = UiBuilder::new()
        .chat_with_width(history, width)
        .into_history();
    wrap_lines(&history, width)
}

fn build_document(rendered_history_lines: Vec<UiLine>, width: usize, tick: usize) -> UiDocument {
    UiBuilder::new()
        .rendered_history_lines(rendered_history_lines)
        .input(
            &format!("benchmark input {tick}{CURSOR_MARKER}{VISIBLE_CURSOR}"),
            &[],
            0,
        )
        .bottom_status(
            BottomStatus {
                provider: "bench",
                model: "render-frame",
                reasoning_effort: "none",
                context_tokens: tick as u64,
                context_window: 1_000_000,
            },
            width,
        )
        .progress(&crate::ActivityState::idle(), 0, Instant::now(), width)
        .finish()
}
