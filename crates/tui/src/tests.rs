use super::*;
use crate::markdown::{
    render_markdown, MD_BOLD_OFF, MD_BOLD_ON, MD_CODE_ON, MD_DIM_OFF, MD_DIM_ON, MD_ITALIC_OFF,
    MD_ITALIC_ON,
};
use crate::tool_preview::tool_output_preview;
use crossterm::event::{KeyCode, KeyModifiers};
use jucode_agent_core::{ModelOptionView, SessionListItemView, TreeNodeView};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

fn strip_ansi(text: &str) -> String {
    let mut output = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            output.push(ch);
            continue;
        }
        if chars.next() != Some('[') {
            continue;
        }
        for ch in chars.by_ref() {
            if ch.is_ascii_alphabetic() {
                break;
            }
        }
    }
    output
}

fn ansi_visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut rest = text;
    while !rest.is_empty() {
        if let Some((_, next)) = split_ansi_sequence(rest) {
            rest = next;
            continue;
        }
        let Some(ch) = rest.chars().next() else {
            break;
        };
        width += ch.width().unwrap_or(0);
        rest = &rest[ch.len_utf8()..];
    }
    width
}

#[derive(Default)]
struct TestRuntime {
    submitted: Vec<String>,
    commands: Vec<String>,
}

impl TuiRuntime for TestRuntime {
    fn startup_events(&self) -> Vec<AgentEvent> {
        Vec::new()
    }

    fn model_status_event(&self) -> AgentEvent {
        AgentEvent::Status("ready".to_string())
    }

    fn submit_user_message(&mut self, message: String) -> Vec<AgentEvent> {
        self.submitted.push(message.clone());
        vec![AgentEvent::UserMessage(message)]
    }

    fn steer(&mut self) -> Vec<AgentEvent> {
        Vec::new()
    }

    fn handle_command(&mut self, input: &str) -> (bool, Vec<AgentEvent>) {
        self.commands.push(input.to_string());
        (false, Vec::new())
    }

    fn poll_events(&mut self) -> Vec<AgentEvent> {
        Vec::new()
    }
}

#[test]
fn paste_normalizes_newlines_without_submitting() {
    let mut app = TuiApp::new(TestRuntime::default());

    app.handle_paste("hello\r\nworld");

    assert_eq!(app.input.text(), "hello\nworld");
    assert!(app.runtime.submitted.is_empty());
}

#[test]
fn modified_enter_inserts_newline_and_plain_enter_submits_once() {
    let mut app = TuiApp::new(TestRuntime::default());
    let now = Instant::now();

    app.handle_key_at(KeyCode::Char('a'), KeyModifiers::empty(), now);
    app.handle_key_at(
        KeyCode::Enter,
        KeyModifiers::SHIFT,
        now + PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(1),
    );
    app.handle_key_at(
        KeyCode::Char('b'),
        KeyModifiers::empty(),
        now + PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(2),
    );
    app.handle_key_at(
        KeyCode::Enter,
        KeyModifiers::empty(),
        now + PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(11),
    );

    assert_eq!(app.runtime.submitted, vec!["a\nb".to_string()]);
}

#[test]
fn ctrl_enter_inserts_newline() {
    let mut app = TuiApp::new(TestRuntime::default());
    let now = Instant::now();

    app.handle_key_at(KeyCode::Char('a'), KeyModifiers::empty(), now);
    app.handle_key_at(
        KeyCode::Enter,
        KeyModifiers::CONTROL,
        now + Duration::from_millis(1),
    );
    app.handle_key_at(
        KeyCode::Char('b'),
        KeyModifiers::empty(),
        now + Duration::from_millis(2),
    );
    app.flush_paste_burst_if_due(now + PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(3));

    assert_eq!(app.input.text(), "a\nb");
    assert!(app.runtime.submitted.is_empty());
}

#[test]
fn paste_burst_keeps_multiline_text_in_input() {
    let mut app = TuiApp::new(TestRuntime::default());
    let now = Instant::now();

    for (index, ch) in "hello".chars().enumerate() {
        app.handle_key_at(
            KeyCode::Char(ch),
            KeyModifiers::empty(),
            now + Duration::from_millis(index as u64),
        );
    }
    app.handle_key_at(
        KeyCode::Enter,
        KeyModifiers::empty(),
        now + Duration::from_millis(5),
    );
    for (index, ch) in "world".chars().enumerate() {
        app.handle_key_at(
            KeyCode::Char(ch),
            KeyModifiers::empty(),
            now + Duration::from_millis(6 + index as u64),
        );
    }

    assert!(app.runtime.submitted.is_empty());
    app.flush_paste_burst_if_due(now + PASTE_BURST_IDLE_TIMEOUT + Duration::from_millis(20));

    assert_eq!(app.input.text(), "hello\nworld");
    assert!(app.runtime.submitted.is_empty());
}

#[test]
fn paste_burst_large_text_uses_placeholder() {
    let mut app = TuiApp::new(TestRuntime::default());
    let now = Instant::now();
    let pasted = "x".repeat(PASTE_PLACEHOLDER_CHARS + 1);

    for (index, ch) in pasted.chars().enumerate() {
        app.handle_key_at(
            KeyCode::Char(ch),
            KeyModifiers::empty(),
            now + Duration::from_millis(index as u64),
        );
    }
    app.flush_paste_burst_if_due(
        now + Duration::from_millis(pasted.len() as u64) + PASTE_BURST_IDLE_TIMEOUT,
    );

    assert_eq!(
        app.input.display_text(),
        format!("[Pasted: {} chars]", PASTE_PLACEHOLDER_CHARS + 1)
    );
    assert_eq!(app.input.text(), pasted);
}

#[test]
fn paste_burst_render_tick_skips_until_pending_char_flushes() {
    let mut app = TuiApp::new(TestRuntime::default());
    let now = Instant::now();
    let mut frames = FrameScheduler {
        next_frame_at: None,
    };

    app.handle_key_at(KeyCode::Char('a'), KeyModifiers::empty(), now);

    assert_eq!(app.input.text(), "");
    assert!(app.paste_burst.is_active());
    assert!(app.handle_paste_burst_render_tick(now, &mut frames));
    assert_eq!(app.input.text(), "");
    assert_eq!(frames.next_frame_at, Some(now + paste_burst_render_delay()));

    assert!(app.handle_paste_burst_render_tick(now + paste_burst_render_delay(), &mut frames));
    assert_eq!(app.input.text(), "a");
    assert_eq!(frames.next_frame_at, Some(now + paste_burst_render_delay()));
    assert!(!app.handle_paste_burst_render_tick(now + paste_burst_render_delay(), &mut frames));
}

#[test]
fn single_typed_char_flushes_after_burst_window() {
    let mut app = TuiApp::new(TestRuntime::default());
    let now = Instant::now();

    app.handle_key_at(KeyCode::Char('a'), KeyModifiers::empty(), now);
    assert_eq!(app.input.text(), "");

    app.flush_paste_burst_if_due(now + PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(1));

    assert_eq!(app.input.text(), "a");
}

#[test]
fn input_renders_multiple_lines() {
    let document = UiBuilder::new().input("one\ntwo", &[], 0).finish();

    assert_eq!(document.controls[0].kind, UiKind::Input);
    assert_eq!(document.controls[0].text, "");
    assert_eq!(document.controls[1].kind, UiKind::Input);
    assert_eq!(document.controls[1].text, "> one");
    assert_eq!(document.controls[2].kind, UiKind::Input);
    assert_eq!(document.controls[2].text, "  two");
    assert_eq!(document.controls[3].kind, UiKind::Input);
    assert_eq!(document.controls[3].text, "");
}

#[test]
fn single_line_input_renders_text_on_middle_row() {
    let document = UiBuilder::new().input("hello", &[], 0).finish();

    assert_eq!(document.controls.len(), 3);
    assert_eq!(document.controls[0].kind, UiKind::Input);
    assert_eq!(document.controls[0].text, "");
    assert_eq!(document.controls[1].kind, UiKind::Input);
    assert_eq!(document.controls[1].text, "> hello");
    assert_eq!(document.controls[2].kind, UiKind::Input);
    assert_eq!(document.controls[2].text, "");
}

#[test]
fn cursor_row_is_relative_to_whole_frame() {
    let frame = RenderedFrame::build(
        &UiDocument {
            history: vec![
                UiLine {
                    kind: UiKind::User,
                    text: "hello".to_string(),
                },
                UiLine {
                    kind: UiKind::Assistant,
                    text: "world".to_string(),
                },
            ],
            rendered_history_lines: None,
            controls: vec![UiLine {
                kind: UiKind::Input,
                text: format!("> prompt{CURSOR_MARKER}"),
            }],
            reset_screen: false,
        },
        80,
    );

    let cursor = frame.cursor.expect("cursor marker should be found");
    assert_eq!(cursor.row, 3);
    assert_eq!(cursor.column, 8 + CONTENT_LEFT_PADDING);
}

#[test]
fn command_completion_renders_below_input_with_selected_color() {
    let document = UiBuilder::new()
        .input(
            &format!("/{CURSOR_MARKER}{VISIBLE_CURSOR}"),
            &[
                CommandCandidate {
                    command: "/help".to_string(),
                    marker: None,
                },
                CommandCandidate {
                    command: "/review".to_string(),
                    marker: Some("SKILL".to_string()),
                },
            ],
            1,
        )
        .finish();

    assert_eq!(document.controls.len(), 5);
    assert_eq!(document.controls[0].kind, UiKind::Input);
    assert_eq!(document.controls[0].text, "");
    assert_eq!(document.controls[1].kind, UiKind::Input);
    assert_eq!(
        document.controls[1].text,
        format!("> /{CURSOR_MARKER}{VISIBLE_CURSOR}")
    );
    assert_eq!(document.controls[2].kind, UiKind::Input);
    assert_eq!(document.controls[2].text, "");
    assert_eq!(document.controls[3].kind, UiKind::Status);
    assert_eq!(document.controls[3].text, "  /help");
    assert_eq!(document.controls[4].kind, UiKind::Selected);
    assert_eq!(document.controls[4].text, "  /review SKILL");
}

#[test]
fn model_and_tokens_render_below_input_without_ready_status() {
    let document = UiBuilder::new()
        .input(&format!("hello{CURSOR_MARKER}{VISIBLE_CURSOR}"), &[], 0)
        .bottom_status(
            BottomStatus {
                provider: "openai",
                model: "gpt-5",
                reasoning_effort: "medium",
                context_tokens: 12_345,
                context_window: 400_000,
            },
            padded_content_width(64),
        )
        .finish();

    assert_eq!(document.controls.len(), 4);
    assert_eq!(
        document.controls[1].text,
        format!("> hello{CURSOR_MARKER}{VISIBLE_CURSOR}")
    );
    let status = strip_ansi(&document.controls[3].text);
    assert!(status.starts_with("openai / gpt-5 (medium)"));
    assert!(status.ends_with("tokens 12345/400000 | context 3.1%"));
    assert!(!status.contains("ready"));
    assert_eq!(
        UnicodeWidthStr::width(strip_ansi(&document.controls[3].text).as_str()),
        padded_content_width(64)
    );
}

#[test]
fn input_background_extends_across_frame_width() {
    let document = UiBuilder::new()
        .input(&format!("hi{CURSOR_MARKER}{VISIBLE_CURSOR}"), &[], 0)
        .finish();

    let frame = RenderedFrame::build(&document, 40);
    let input_line = frame
        .lines
        .iter()
        .find(|line| strip_ansi(line).contains("> hi"))
        .expect("input line should render");

    assert!(input_line.contains("\x1b[38;2;224;226;232;48;2;48;52;62m"));
    assert_eq!(UnicodeWidthStr::width(strip_ansi(input_line).as_str()), 40);
}

#[test]
fn native_cursor_tracks_middle_input_row() {
    let document = UiBuilder::new()
        .input(&format!("hello{CURSOR_MARKER}{VISIBLE_CURSOR}"), &[], 0)
        .finish();

    let frame = RenderedFrame::build(&document, 40);
    let cursor = frame.cursor.expect("cursor marker should be found");

    assert_eq!(frame.lines.len(), 3);
    assert!(strip_ansi(&frame.lines[cursor.row]).starts_with("  > hello|"));
    assert_eq!(
        UnicodeWidthStr::width(strip_ansi(&frame.lines[cursor.row]).as_str()),
        40
    );
    assert_eq!(cursor.row, 1);
    assert_eq!(cursor.column, CONTENT_LEFT_PADDING + 2 + "hello".len());
}

#[test]
fn progress_renders_above_input() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.apply_events(vec![AgentEvent::Connecting]);

    let document = app.build_document(80, Instant::now());
    let progress_index = document
        .controls
        .iter()
        .position(|line| line.text.contains("connecting"))
        .expect("progress line should render");
    let input_index = document
        .controls
        .iter()
        .position(|line| line.kind == UiKind::Input)
        .expect("input line should render");

    assert!(progress_index < input_index);
}

#[test]
fn colored_status_line_does_not_wrap_at_visible_width() {
    let document = UiBuilder::new()
        .input("", &[], 0)
        .bottom_status(
            BottomStatus {
                provider: "jucode",
                model: "claude-opus-4.7",
                reasoning_effort: "high",
                context_tokens: 1633,
                context_window: 400_000,
            },
            padded_content_width(64),
        )
        .finish();

    let frame = RenderedFrame::build(&document, 64);

    assert_eq!(frame.lines.len(), 4);
    assert!(strip_ansi(&frame.lines[3]).contains("tokens 1633/400000 | context 0.4%"));
    assert_eq!(
        UnicodeWidthStr::width(strip_ansi(&frame.lines[3]).as_str()),
        64
    );
}

#[test]
fn startup_renders_inside_box() {
    let document = UiBuilder::new()
        .chat(&[ChatLine::Startup {
            version: "0.1.2".to_string(),
            profile_dir: "C:\\Users\\me\\.jucode".to_string(),
            config_path: "E:\\Code\\Projects\\JuCode\\JuCode-CLI".to_string(),
            cwd: "C:\\Users\\me\\projects\\jucode".to_string(),
            model: "claude-opus-4-7".to_string(),
            context_window: 1_000_000,
        }])
        .finish();

    assert!(document.history[0].text.starts_with("\x1b[90m╭"));
    assert!(document.history[1].text.contains("\x1b[90m│"));
    assert!(strip_ansi(&document.history[0].text).starts_with('╭'));
    assert!(strip_ansi(&document.history[1].text).contains(" \\/"));
    assert!(strip_ansi(&document.history[1].text)
        .contains("Welcome to JuCode (claude-opus-4-7 · 1M context)"));
    assert!(strip_ansi(&document.history[2].text).contains("<'l"));
    assert!(!strip_ansi(&document.history[2].text).contains("cwd:"));
    assert!(strip_ansi(&document.history[3].text).contains(" ll"));
    assert!(strip_ansi(&document.history[3].text).contains("cwd:"));
    assert!(!document
        .history
        .iter()
        .any(|line| strip_ansi(&line.text).contains("directory:")));
    assert!(strip_ansi(&document.history[5].text).contains(" || ||"));
    assert!(strip_ansi(&document.history[5].text).contains("/help for commands"));
    assert!(strip_ansi(&document.history[7].text).starts_with('╰'));
    let border_width = ansi_visible_width(&document.history[0].text);
    for line in document.history.iter().take(8) {
        assert_eq!(
            ansi_visible_width(&line.text),
            border_width,
            "{}",
            strip_ansi(&line.text)
        );
    }
}

#[test]
fn connecting_progress_uses_continuous_color_gradient() {
    let now = Instant::now();
    let mut activity = ActivityState::idle();
    activity.kind = ActivityKind::Connecting;
    activity.turn_started_at = Some(now - Duration::from_secs(11));
    activity.phase_started_at = Some(now - Duration::from_secs(11));

    assert_eq!(activity.progress(now, 0).unwrap().color, (255, 60, 60));

    activity.phase_started_at = Some(now - Duration::from_secs(5));
    assert_eq!(activity.progress(now, 0).unwrap().color, (255, 210, 0));

    activity.phase_started_at = Some(now - Duration::from_secs(1));
    assert_eq!(activity.progress(now, 0).unwrap().color, (255, 246, 204));
}

#[test]
fn connecting_event_then_thinking_event_switch_states() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.apply_events(vec![AgentEvent::Connecting]);
    assert!(matches!(app.state.activity.kind, ActivityKind::Connecting));
    app.apply_events(vec![AgentEvent::ThinkingStart]);
    assert!(matches!(app.state.activity.kind, ActivityKind::Thinking));
}

fn reasoning_entry(app: &TuiApp<TestRuntime>) -> Option<(String, bool)> {
    app.state.chat.iter().find_map(|line| match line {
        ChatLine::Reasoning { text, collapsed } => Some((text.clone(), *collapsed)),
        _ => None,
    })
}

#[test]
fn reasoning_streams_into_transcript_then_collapses() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.apply_events(vec![
        AgentEvent::Connecting,
        AgentEvent::ThinkingStart,
        AgentEvent::ReasoningDelta("Let me think".to_string()),
        AgentEvent::ReasoningDelta(" about it.".to_string()),
    ]);
    // Reasoning is a transcript message, streaming, not collapsed yet.
    assert_eq!(
        reasoning_entry(&app),
        Some(("Let me think about it.".to_string(), false))
    );
    assert!(matches!(app.state.activity.kind, ActivityKind::Thinking));

    app.apply_events(vec![AgentEvent::AssistantDelta("Answer".to_string())]);
    // Kept as a message, now collapsed.
    assert_eq!(
        reasoning_entry(&app),
        Some(("Let me think about it.".to_string(), true))
    );
    assert!(matches!(app.state.activity.kind, ActivityKind::Output));
}

#[test]
fn reasoning_tokens_show_in_thinking_progress_not_transcript() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.apply_events(vec![
        AgentEvent::ThinkingStart,
        AgentEvent::ReasoningDelta("thinking".to_string()),
        AgentEvent::Usage {
            input_tokens: 5,
            output_tokens: 2,
            reasoning_tokens: 88,
        },
    ]);
    assert_eq!(app.state.thinking_tokens, 88);
    let document = app.build_document(80, Instant::now());
    // Token count is in the progress line, not the transcript.
    assert!(document
        .controls
        .iter()
        .any(|line| line.text.contains("thinking") && line.text.contains("(88 tokens)")));
    assert!(!document
        .history
        .iter()
        .any(|line| line.text.contains("88 tokens")));
}

#[test]
fn markdown_heading_renders_bold() {
    let base = color_code(UiKind::Assistant);
    assert_eq!(
        render_markdown("## Section title", usize::MAX, base),
        vec![format!("{MD_BOLD_ON}Section title{MD_BOLD_OFF}")]
    );
}

#[test]
fn markdown_bold_and_italic_render_inline() {
    let base = color_code(UiKind::Assistant);
    assert_eq!(
        render_markdown("a **bold** and *em* word", usize::MAX, base),
        vec![format!(
            "a {MD_BOLD_ON}bold{MD_BOLD_OFF} and {MD_ITALIC_ON}em{MD_ITALIC_OFF} word"
        )]
    );
}

#[test]
fn markdown_inline_code_recolors_and_restores_base() {
    let base = color_code(UiKind::Assistant);
    // Inline code uses a foreground color (not a background), restored to base.
    assert_eq!(
        render_markdown("run `a*b*c` now", usize::MAX, base),
        vec![format!("run {MD_CODE_ON}a*b*c{base} now")]
    );
}

#[test]
fn markdown_fenced_code_block_renders_verbatim_with_gutter() {
    let md = "before\n```rust\nlet x = **2**;\nfoo();\n```\nafter";
    let base = color_code(UiKind::Assistant);
    assert_eq!(
        render_markdown(md, usize::MAX, base),
        vec![
            "before".to_string(),
            format!("{MD_DIM_ON}│ let x = **2**;{MD_DIM_OFF}"),
            format!("{MD_DIM_ON}│ foo();{MD_DIM_OFF}"),
            "after".to_string(),
        ]
    );
}

#[test]
fn markdown_unbalanced_markers_stay_literal() {
    let base = color_code(UiKind::Assistant);
    assert_eq!(
        render_markdown("2 * 3 = 6", usize::MAX, base),
        vec!["2 * 3 = 6"]
    );
}

#[test]
fn markdown_table_renders_aligned_box() {
    let table = "| Name | Qty |\n|:-----|----:|\n| apple | 3 |\n| fig | 22 |";
    let lines = render_markdown(table, usize::MAX, color_code(UiKind::Assistant));
    let plain: Vec<String> = lines.iter().map(|line| strip_ansi(line)).collect();

    assert_eq!(
        plain,
        vec![
            "┌───────┬─────┐".to_string(),
            "│ Name  │ Qty │".to_string(), // left-aligned header
            "├───────┼─────┤".to_string(),
            "│ apple │   3 │".to_string(), // Qty right-aligned
            "│ fig   │  22 │".to_string(),
            "└───────┴─────┘".to_string(),
        ]
    );
    // Header cells are bold (styling preserved before strip).
    assert!(lines[1].contains(&format!("{MD_BOLD_ON}Name{MD_BOLD_OFF}")));
}

#[test]
fn markdown_table_is_capped_to_width() {
    let table = "| A | B |\n|---|---|\n| xxxxxxxxxx | yyyyyyyyyy |";
    let lines = render_markdown(table, 20, color_code(UiKind::Assistant));
    for line in &lines {
        assert!(
            visible_width(line) <= 20,
            "line exceeds width: {}",
            strip_ansi(line)
        );
    }
}

#[test]
fn assistant_message_is_rendered_as_markdown() {
    let document = UiBuilder::new()
        .chat(&[ChatLine::Assistant("# Hi **there**".to_string())])
        .finish();
    assert!(document
        .history
        .iter()
        .any(|line| line.text.contains(MD_BOLD_ON) && line.text.contains("there")));
}

#[test]
fn thinking_tokens_reset_after_reply_completes() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.apply_events(vec![
        AgentEvent::ThinkingStart,
        AgentEvent::ReasoningDelta("thinking".to_string()),
        AgentEvent::AssistantDelta("answer".to_string()),
        AgentEvent::Usage {
            input_tokens: 1,
            output_tokens: 1,
            reasoning_tokens: 42,
        },
    ]);
    assert_eq!(app.state.thinking_tokens, 42);

    app.apply_events(vec![AgentEvent::Status("ready".to_string())]);
    // Progress token state is cleared; the reasoning message itself stays in chat.
    assert_eq!(app.state.thinking_tokens, 0);
    let document = app.build_document(80, Instant::now());
    assert!(!document
        .controls
        .iter()
        .any(|line| line.text.contains("42 tokens")));
    assert!(reasoning_entry(&app).is_some());
}

#[test]
fn collapsed_reasoning_message_keeps_only_first_lines() {
    let document = UiBuilder::new()
        .chat(&[ChatLine::Reasoning {
            text: "l1\nl2\nl3\nl4\nl5".to_string(),
            collapsed: true,
        }])
        .finish();
    let body: Vec<&str> = document
        .history
        .iter()
        .filter(|line| line.text.starts_with("  "))
        .map(|line| line.text.trim())
        .collect();
    assert_eq!(body, vec!["l1", "l2", "l3", "…"]);
}

#[test]
fn compaction_events_set_progress_notices_and_context_meter() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.apply_events(vec![AgentEvent::ContextUsage { tokens: 900_000 }]);
    assert_eq!(app.state.current_context_tokens, 900_000);

    app.apply_events(vec![AgentEvent::CompactionStart]);
    assert!(matches!(app.state.activity.kind, ActivityKind::Compacting));
    assert!(app
        .state
        .chat
        .iter()
        .any(|line| matches!(line, ChatLine::System(text) if text.contains("Compacting"))));

    app.apply_events(vec![AgentEvent::CompactionProgress { output_tokens: 42 }]);
    assert_eq!(app.state.activity.estimated_output_tokens, 42);

    app.apply_events(vec![
        AgentEvent::CompactionEnd,
        AgentEvent::ContextUsage { tokens: 25_000 },
    ]);
    assert!(app
        .state
        .chat
        .iter()
        .any(|line| matches!(line, ChatLine::System(text) if text.contains("compacted"))));
    assert_eq!(app.state.current_context_tokens, 25_000);
}

#[test]
fn output_progress_color_warms_as_stream_stalls() {
    let now = Instant::now();
    let mut activity = ActivityState::idle();
    activity.kind = ActivityKind::Output;
    activity.turn_started_at = Some(now - Duration::from_secs(6));
    activity.phase_started_at = Some(now - Duration::from_secs(6));
    activity.estimated_output_tokens = 12;

    activity.last_delta_at = Some(now);
    let fresh = activity.progress(now, 0).unwrap().color;
    activity.last_delta_at = Some(now - Duration::from_secs(3));
    let stalled = activity.progress(now, 0).unwrap().color;

    assert_eq!(fresh, (70, 220, 110));
    assert!(stalled.0 > fresh.0);
    assert!(stalled.2 < fresh.2);
}

#[test]
fn spinner_presets_are_single_ascii_chars() {
    let presets = [
        SpinnerPreset::Line,
        SpinnerPreset::Dots,
        SpinnerPreset::Pulse,
        SpinnerPreset::Scan,
    ];

    for preset in presets {
        let value = spinner_char(preset, 1, 1);
        assert_eq!(value.len(), 1);
        assert!(value.is_ascii());
    }
}

#[test]
fn tool_output_preview_truncates_long_output() {
    let output = (0..20)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let preview = tool_output_preview("custom", &output, false);

    assert!(preview.contains("line 0"));
    assert!(!preview.contains("line 19"));
    assert!(preview.contains('…'));
}

#[test]
fn tool_output_preview_prefers_diff_field() {
    let output = serde_json::json!({
        "stdout": "raw",
        "diff": "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n"
    })
    .to_string();

    let preview = tool_output_preview("str_replace", &output, false);
    let visible_preview = strip_ansi(&preview);

    assert!(preview.contains("* Edited a (+1 -1)"));
    assert!(visible_preview.contains("1 -  old"));
    assert!(visible_preview.contains("1 +  new"));
    assert!(!preview.contains("diff --git a/a b/a"));
    assert!(!visible_preview.contains("raw"));
}

#[test]
fn tool_output_preview_keeps_additions_after_large_removals() {
    let removals = (0..30)
        .map(|index| format!("-old line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let diff = format!(
            "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1,30 +1,2 @@\n{removals}\n+new important line\n+another important line\n"
        );
    let output = serde_json::json!({
        "stdout": "raw",
        "diff": diff
    })
    .to_string();

    let preview = tool_output_preview("edit", &output, false);

    assert!(preview.contains("* Edited README.md (+2 -30)"));
    assert!(preview.contains("     1 -  old line 0"));
    assert!(preview.contains("    30 -  old line 29"));
    assert!(preview.contains("     1 +  new important line"));
    assert!(preview.contains("     2 +  another important line"));
    assert!(!preview.contains("--- a/README.md"));
    assert!(!preview.contains("+++ b/README.md"));
    assert!(!preview.contains('…'));
}

#[test]
fn tool_output_preview_projects_read_and_ls() {
    let read = serde_json::json!({
        "path": "C:\\repo\\src\\main.rs",
        "offset": 5,
        "lines_read": 12,
        "content": "hidden"
    })
    .to_string();
    let ls = serde_json::json!({
        "path": "C:\\repo\\src",
        "entries": ["main.rs"]
    })
    .to_string();

    let read_preview = tool_output_preview("read", &read, false);
    let ls_preview = tool_output_preview("ls", &ls, false);

    assert_eq!(read_preview, "read main.rs: 12 lines from line 5");
    assert!(!read_preview.contains("hidden"));
    assert_eq!(ls_preview, "ls C:\\repo\\src");
}

#[test]
fn tool_output_preview_projects_bash_latest_logs() {
    let output = serde_json::json!({
        "command": "cargo test",
        "exit_code": 0,
        "stdout": "one\ntwo\nthree\n",
        "stderr": "",
        "timed_out": false
    })
    .to_string();

    let preview = tool_output_preview("bash", &output, false);
    let plain_preview = strip_ansi(&preview);

    assert!(plain_preview.contains("cargo test  exit 0"));
    assert!(plain_preview.contains("stdout:"));
    assert!(plain_preview.contains("three"));
}

#[test]
fn chat_history_inserts_separator_between_turns() {
    let document = UiBuilder::new()
        .chat(&[
            ChatLine::User("first".to_string()),
            ChatLine::Assistant("second".to_string()),
        ])
        .finish();

    assert!(document
        .history
        .iter()
        .any(|line| line.kind == UiKind::Separator && line.text.starts_with('─')));
}

#[test]
fn tool_preview_colors_diff_lines() {
    let document = UiBuilder::new()
        .chat(&[ChatLine::Tool {
            call_id: None,
            name: "edit".to_string(),
            output: serde_json::json!({
                "diff": "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n"
            })
            .to_string(),
            running: false,
        }])
        .finish();

    assert!(
        document
            .history
            .iter()
            .any(|line| line.kind == UiKind::DiffRemove
                && strip_ansi(&line.text).contains("1 -  old"))
    );
    assert!(document
        .history
        .iter()
        .any(|line| line.kind == UiKind::DiffAdd && strip_ansi(&line.text).contains("1 +  new")));
}

#[test]
fn rendered_frame_keeps_full_history_for_native_scrollback() {
    let document = UiBuilder::new().finish_with_history_and_input(20);

    let frame = RenderedFrame::build(&document, 80);
    let output = frame.lines.join("\n");

    assert!(output.contains("line 0"));
    assert!(output.contains("line 19"));
    assert_eq!(frame.lines.len(), 24);
}

#[test]
fn projection_keeps_live_assistant_out_of_transcript() {
    let document = UiBuilder::new()
        .chat(&[ChatLine::User("hello".to_string())])
        .live_assistant(Some("streaming"), 80)
        .input("", &[], 0)
        .finish();

    let projection = ProjectedDocument::from_document(&document, 80);

    assert!(projection
        .transcript_lines
        .iter()
        .any(|line| line.contains("hello")));
    assert!(!projection
        .transcript_lines
        .iter()
        .any(|line| line.contains("streaming")));
    assert!(projection
        .active_lines
        .iter()
        .any(|line| line.contains("streaming")));
}

#[test]
fn cursor_row_accounts_for_full_history() {
    let document = UiBuilder::new().finish_with_history_and_input(20);

    let frame = RenderedFrame::build(&document, 80);
    let cursor = frame.cursor.expect("cursor marker should be found");

    assert_eq!(frame.lines.len(), 24);
    assert_eq!(cursor.row, 22);
    assert_eq!(cursor.column, 2 + CONTENT_LEFT_PADDING);
}

#[test]
fn checkout_tree_enter_maps_to_checkout_command() {
    let mut tree = PickerState::checkout(vec![TreeNodeView {
        id: "e3".to_string(),
        parent_id: None,
        label: "selected prompt".to_string(),
        active: true,
    }]);

    assert_eq!(tree.selected_command().as_deref(), Some("/checkout e3"));

    tree.begin_tree_prompt(TreePromptAction::Fork);
    for ch in "feature".chars() {
        tree.push_prompt_char(ch);
    }
    assert_eq!(tree.take_prompt_command().as_deref(), Some("/fork feature"));
}

#[test]
fn checkout_tree_fork_and_delete_are_interactive_commands() {
    let mut app = TuiApp::new(TestRuntime::default());
    let now = Instant::now();
    app.state.picker_view = Some(PickerState::checkout(vec![TreeNodeView {
        id: "e3".to_string(),
        parent_id: None,
        label: "selected prompt".to_string(),
        active: true,
    }]));

    app.handle_key_at(KeyCode::Char('f'), KeyModifiers::empty(), now);
    for (index, ch) in "feature".chars().enumerate() {
        app.handle_key_at(
            KeyCode::Char(ch),
            KeyModifiers::empty(),
            now + Duration::from_millis(index as u64 + 1),
        );
    }
    app.handle_key_at(
        KeyCode::Enter,
        KeyModifiers::empty(),
        now + Duration::from_millis(20),
    );

    app.handle_key_at(
        KeyCode::Delete,
        KeyModifiers::empty(),
        now + Duration::from_millis(30),
    );
    for (index, ch) in "feature".chars().enumerate() {
        app.handle_key_at(
            KeyCode::Char(ch),
            KeyModifiers::empty(),
            now + Duration::from_millis(index as u64 + 31),
        );
    }
    app.handle_key_at(
        KeyCode::Enter,
        KeyModifiers::empty(),
        now + Duration::from_millis(50),
    );

    assert_eq!(
        app.runtime.commands,
        vec!["/fork feature".to_string(), "/delete feature".to_string()]
    );
}

#[test]
fn checkout_tree_expands_sparse_history_fully() {
    // A mostly-linear history has little branching, so it expands all the way.
    let tree = PickerState::checkout(vec![
        TreeNodeView {
            id: "e1".to_string(),
            parent_id: None,
            label: "first".to_string(),
            active: false,
        },
        TreeNodeView {
            id: "e2".to_string(),
            parent_id: Some("e1".to_string()),
            label: "second".to_string(),
            active: false,
        },
        TreeNodeView {
            id: "e3".to_string(),
            parent_id: Some("e2".to_string()),
            label: "third".to_string(),
            active: false,
        },
    ]);

    assert_eq!(
        tree.rows
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        vec!["e1", "e2", "e3"]
    );
    assert!(tree.rows[0].prefix.contains("──"));
}

fn wide_tree_nodes() -> Vec<TreeNodeView> {
    let mut nodes = vec![TreeNodeView {
        id: "e1".to_string(),
        parent_id: None,
        label: "root".to_string(),
        active: false,
    }];
    for index in 2..=20 {
        nodes.push(TreeNodeView {
            id: format!("e{index}"),
            parent_id: Some("e1".to_string()),
            label: format!("child {index}"),
            active: false,
        });
    }
    nodes.push(TreeNodeView {
        id: "c1".to_string(),
        parent_id: Some("e2".to_string()),
        label: "grandchild".to_string(),
        active: false,
    });
    nodes
}

#[test]
fn checkout_tree_limits_expansion_when_branching_is_wide() {
    // Wide branching fills the row budget early, so deeper levels stay collapsed.
    let tree = PickerState::checkout(wide_tree_nodes());

    assert!(!tree.rows.iter().any(|row| row.id == "c1"));
    assert_eq!(tree.rows.len(), 20); // root + 19 children, grandchild hidden
}

#[test]
fn fill_input_event_populates_input_box() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.input.push_text("stale");
    app.apply_events(vec![AgentEvent::FillInput("resend this".to_string())]);
    assert_eq!(app.input.text(), "resend this");
}

#[test]
fn checkout_tree_marks_rows_with_children_as_directories() {
    // Wide tree: the root is expanded ([-]); a child with hidden descendants
    // stays collapsed ([+]).
    let tree = PickerState::checkout(wide_tree_nodes());
    let document = UiBuilder::new().picker(Some(&tree)).finish();

    assert!(document
        .controls
        .iter()
        .any(|line| line.text.contains("[-] root")));
    assert!(document
        .controls
        .iter()
        .any(|line| line.kind == UiKind::TreeDirectory && line.text.contains("[+] child 2")));
}

#[test]
fn resume_picker_enter_maps_to_resume_command_without_delete() {
    let tree = PickerState::resume(vec![SessionListItemView {
        id: "s123".to_string(),
        label: "Fix resume list".to_string(),
        detail: "working · summarize current task".to_string(),
        active: false,
    }]);

    assert_eq!(tree.selected_command().as_deref(), Some("/resume s123"));
}

#[test]
fn model_picker_enter_includes_selected_effort() {
    let mut picker = PickerState::model(
        vec![
            ModelOptionView {
                model: "gpt-5.2".to_string(),
                active: false,
                context_window: 400_000,
                max_output_tokens: 128_000,
                reasoning_efforts: vec!["none".to_string(), "low".to_string()],
            },
            ModelOptionView {
                model: "gpt-5.3-codex".to_string(),
                active: true,
                context_window: 400_000,
                max_output_tokens: 128_000,
                reasoning_efforts: vec![
                    "low".to_string(),
                    "medium".to_string(),
                    "high".to_string(),
                    "xhigh".to_string(),
                ],
            },
        ],
        "low".to_string(),
    );

    assert_eq!(
        picker.selected_command().as_deref(),
        Some("/model gpt-5.3-codex low")
    );

    picker.cycle_effort();

    assert_eq!(
        picker.selected_command().as_deref(),
        Some("/model gpt-5.3-codex medium")
    );
}

#[test]
fn model_picker_renders_effort_hint() {
    let picker = PickerState::model(
        vec![ModelOptionView {
            model: "gpt-5.2".to_string(),
            active: true,
            context_window: 400_000,
            max_output_tokens: 128_000,
            reasoning_efforts: vec!["none".to_string(), "low".to_string()],
        }],
        "none".to_string(),
    );
    let document = UiBuilder::new().picker(Some(&picker)).finish();
    let controls = document
        .controls
        .iter()
        .map(|line| strip_ansi(&line.text))
        .collect::<Vec<_>>();

    assert!(controls.iter().any(|text| text.contains("thinking: none")));
    assert!(controls
        .iter()
        .any(|text| text.contains("gpt-5.2") && text.contains(" *")));
}

#[test]
fn shift_tab_effort_cycle_wraps() {
    let efforts = vec!["none".to_string(), "low".to_string(), "medium".to_string()];
    assert_eq!(next_reasoning_effort(&efforts, "none"), "low");
    assert_eq!(next_reasoning_effort(&efforts, "low"), "medium");
    assert_eq!(next_reasoning_effort(&efforts, "medium"), "none");
    assert_eq!(next_reasoning_effort(&efforts, "unknown"), "none");
}

trait TestUiBuilderExt {
    fn finish_with_history_and_input(self, history_lines: usize) -> UiDocument;
}

impl TestUiBuilderExt for UiBuilder {
    fn finish_with_history_and_input(mut self, history_lines: usize) -> UiDocument {
        for index in 0..history_lines {
            self.history_line(UiKind::Assistant, format!("line {index}"));
        }
        self.input(&format!("{CURSOR_MARKER}{VISIBLE_CURSOR}"), &[], 0)
            .finish()
    }
}
