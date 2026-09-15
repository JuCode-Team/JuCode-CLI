use super::*;
use crate::markdown::{render_markdown, MD_CODE, MD_DIM};
use crate::tool_preview::{format_tool_header, tool_output_preview};
use jucode_agent_core::{ModelOptionView, SessionListItemView, TreeNodeView};
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use ratatui::style::Modifier;
use unicode_width::UnicodeWidthStr;

fn markdown_text(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

fn preview_text(lines: &[UiLine]) -> String {
    lines
        .iter()
        .map(UiLine::plain)
        .collect::<Vec<_>>()
        .join("\n")
}

fn has_modifier(line: &Line<'static>, modifier: Modifier) -> bool {
    line.spans
        .iter()
        .any(|span| span.style.add_modifier.contains(modifier))
}

fn input_lines(text: &str) -> Vec<UiLine> {
    let mut input = crate::input::InputBuffer::default();
    input.push_text(text);
    input.render(true)
}

#[derive(Default)]
struct TestRuntime {
    submitted: Vec<String>,
    commands: Vec<String>,
    interrupts: usize,
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

    fn interrupt(&mut self) -> Vec<AgentEvent> {
        self.interrupts += 1;
        vec![AgentEvent::Status("interrupted".to_string())]
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
fn esc_interrupts_active_turn_and_clears_draft_when_idle() {
    let mut app = TuiApp::new(TestRuntime::default());
    let now = Instant::now();

    app.input.push_char('x');
    app.apply_events(vec![AgentEvent::Connecting]);

    app.handle_key_at(KeyCode::Esc, KeyModifiers::empty(), now);
    assert_eq!(app.runtime.interrupts, 1);
    // The draft survives an interrupt; the turn is over.
    assert_eq!(app.input.text(), "x");
    assert!(!app.state.activity.is_active());

    app.handle_key_at(KeyCode::Esc, KeyModifiers::empty(), now);
    assert_eq!(app.runtime.interrupts, 1);
    assert_eq!(app.input.text(), "");
}

#[test]
fn input_renders_multiple_lines() {
    let document = UiBuilder::new()
        .input(&input_lines("one\ntwo"), &[], 0)
        .finish();

    assert_eq!(document.controls[0].kind, UiKind::Input);
    assert_eq!(document.controls[0].plain(), "");
    assert_eq!(document.controls[1].kind, UiKind::Input);
    assert_eq!(document.controls[1].plain(), "› one");
    assert_eq!(document.controls[2].kind, UiKind::Input);
    assert_eq!(document.controls[2].plain(), "  two");
    assert_eq!(document.controls[3].kind, UiKind::Input);
    assert_eq!(document.controls[3].plain(), "");
}

#[test]
fn single_line_input_renders_text_on_middle_row() {
    let document = UiBuilder::new()
        .input(&input_lines("hello"), &[], 0)
        .finish();

    assert_eq!(document.controls.len(), 3);
    assert_eq!(document.controls[0].kind, UiKind::Input);
    assert_eq!(document.controls[0].plain(), "");
    assert_eq!(document.controls[1].kind, UiKind::Input);
    assert_eq!(document.controls[1].plain(), "› hello");
    assert_eq!(document.controls[2].kind, UiKind::Input);
    assert_eq!(document.controls[2].plain(), "");
}

#[test]
fn cursor_row_is_relative_to_whole_frame() {
    let frame = RenderedFrame::build(
        &UiDocument {
            history: vec![
                UiLine::new(UiKind::User, "hello"),
                UiLine::new(UiKind::Assistant, "world"),
            ],
            rendered_history_lines: None,
            controls: vec![{
                let mut line = UiLine::new(UiKind::Input, "› prompt");
                line.cursor = Some(8);
                line
            }],
            reset_screen: false,
        },
        80,
    );

    let cursor = frame.cursor.expect("cursor marker should be found");
    assert_eq!(cursor.row, 3);
    assert_eq!(cursor.column, 8);
}

#[test]
fn command_completion_renders_below_input_with_selected_color() {
    let document = UiBuilder::new()
        .input(
            &input_lines("/"),
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
    assert_eq!(document.controls[0].plain(), "");
    assert_eq!(document.controls[1].kind, UiKind::Input);
    assert_eq!(document.controls[1].plain(), "› /");
    assert_eq!(document.controls[1].cursor, Some(3));
    assert_eq!(document.controls[2].kind, UiKind::Input);
    assert_eq!(document.controls[2].plain(), "");
    assert_eq!(document.controls[3].kind, UiKind::Status);
    assert_eq!(document.controls[3].plain(), "  /help");
    assert_eq!(document.controls[4].kind, UiKind::Selected);
    assert_eq!(document.controls[4].plain(), "  /review SKILL");
}

#[test]
fn model_and_tokens_render_below_input_without_ready_status() {
    let document = UiBuilder::new()
        .input(&input_lines("hello"), &[], 0)
        .bottom_status(
            BottomStatus {
                provider: "openai",
                model: "gpt-5",
                reasoning_effort: "medium",
                git: None,
                context_tokens: 12_345,
                context_window: 400_000,
                cost: 0.0,
            },
            64,
        )
        .finish();

    assert_eq!(document.controls.len(), 4);
    assert_eq!(document.controls[1].plain(), "› hello");
    assert_eq!(document.controls[1].cursor, Some(7));
    let status = document.controls[3].plain();
    assert!(status.starts_with("openai / gpt-5 (medium)"));
    assert!(status.ends_with("tokens 12345/400000 | context 3.1%"));
    assert!(!status.contains("ready"));
    assert_eq!(document.controls[3].line.width(), 64);
}

#[test]
fn input_line_pads_to_frame_width() {
    let document = UiBuilder::new().input(&input_lines("hi"), &[], 0).finish();

    let frame = RenderedFrame::build(&document, 40);
    let input_line = frame
        .lines
        .iter()
        .find(|line| line.contains("› hi"))
        .expect("input line should render");

    // Projected to the frame, the composer row fills the terminal width.
    assert_eq!(UnicodeWidthStr::width(input_line.as_str()), 40);
}

#[test]
fn native_cursor_tracks_middle_input_row() {
    let document = UiBuilder::new()
        .input(&input_lines("hello"), &[], 0)
        .finish();

    let frame = RenderedFrame::build(&document, 40);
    let cursor = frame.cursor.expect("cursor marker should be found");

    assert_eq!(frame.lines.len(), 3);
    assert!(frame.lines[cursor.row].starts_with("› hello"));
    assert_eq!(cursor.row, 1);
    assert_eq!(cursor.column, 2 + "hello".len());
}

#[test]
fn progress_renders_above_input() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.apply_events(vec![AgentEvent::Connecting]);

    let document = app.build_document(80, Instant::now());
    let progress_index = document
        .controls
        .iter()
        .position(|line| line.plain().contains("connecting"))
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
        .input(&input_lines(""), &[], 0)
        .bottom_status(
            BottomStatus {
                provider: "jucode",
                model: "claude-opus-4.7",
                reasoning_effort: "high",
                git: None,
                context_tokens: 1633,
                context_window: 400_000,
                cost: 0.0,
            },
            64,
        )
        .finish();

    let frame = RenderedFrame::build(&document, 64);

    assert_eq!(frame.lines.len(), 4);
    assert!(frame.lines[3].contains("tokens 1633/400000 | context 0.4%"));
    assert_eq!(UnicodeWidthStr::width(frame.lines[3].as_str()), 64);
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

    assert_eq!(document.history[0].plain().chars().next(), Some('╭'));
    // Box borders keep their dim color as a span style.
    assert!(document.history[0]
        .line
        .spans
        .iter()
        .any(|span| span.style.fg == Some(Color::DarkGray)));
    assert!(document.history[1].plain().contains(" \\/"));
    assert!(document.history[1]
        .plain()
        .contains("Welcome to JuCode v0.1.2 (claude-opus-4-7 · 1M context)"));
    assert!(document.history[2].plain().contains("<'l"));
    assert!(!document.history[2].plain().contains("cwd:"));
    assert!(document.history[3].plain().contains(" ll"));
    assert!(document.history[3].plain().contains("cwd:"));
    assert!(!document
        .history
        .iter()
        .any(|line| line.plain().contains("directory:")));
    assert!(document.history[5].plain().contains(" || ||"));
    assert!(document.history[5].plain().contains("/help for commands"));
    assert_eq!(document.history[7].plain().chars().next(), Some('╰'));
    let border_width = document.history[0].line.width();
    for line in document.history.iter().take(8) {
        assert_eq!(line.line.width(), border_width, "{}", line.plain());
    }
}

#[test]
fn projected_startup_box_lines_stay_aligned() {
    let document = UiBuilder::new()
        .chat(&[ChatLine::Startup {
            version: "0.1.2".to_string(),
            profile_dir: "~/.jucode".to_string(),
            config_path: "~/.jucode/config.toml".to_string(),
            cwd: "~/dev/projects/jucode/JuCode-CLI".to_string(),
            model: "gpt-5.4".to_string(),
            context_window: 1_100_000,
        }])
        .finish();

    let frame = RenderedFrame::build(&document, 80);
    let plain = frame.lines.clone();
    let width = UnicodeWidthStr::width(plain[0].as_str());

    for line in plain.iter().take(8) {
        assert!(
            line.starts_with(['╭', '│', '╰']),
            "startup box line should not be indented: {line:?}"
        );
        assert_eq!(UnicodeWidthStr::width(line.as_str()), width);
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
        ChatLine::Reasoning {
            text, collapsed, ..
        } => Some((text.clone(), *collapsed)),
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
fn thinking_header_click_toggles_reasoning_block() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.apply_events(vec![
        AgentEvent::ThinkingStart,
        AgentEvent::ReasoningDelta("deep thoughts".to_string()),
        AgentEvent::AssistantDelta("answer".to_string()),
    ]);
    let index = app
        .state
        .chat
        .iter()
        .position(|line| matches!(line, ChatLine::Reasoning { .. }))
        .expect("reasoning block");
    // Collapsed by the reply, with its thinking duration recorded.
    assert!(matches!(
        app.state.chat.get(index),
        Some(ChatLine::Reasoning {
            collapsed: true,
            duration_secs: Some(_),
            ..
        })
    ));
    app.state.toggle_reasoning_collapsed(index);
    assert!(matches!(
        app.state.chat.get(index),
        Some(ChatLine::Reasoning {
            collapsed: false,
            ..
        })
    ));
    app.state.toggle_reasoning_collapsed(index);
    assert!(matches!(
        app.state.chat.get(index),
        Some(ChatLine::Reasoning {
            collapsed: true,
            ..
        })
    ));
}

#[test]
fn reasoning_tokens_show_in_thinking_progress_not_transcript() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.apply_events(vec![
        AgentEvent::ThinkingStart,
        AgentEvent::ReasoningDelta("thinking".to_string()),
        AgentEvent::Usage {
            input_tokens: 5,
            cached_input_tokens: 0,
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
        .any(|line| line.plain().contains("thinking") && line.plain().contains("(88 tokens)")));
    assert!(!document
        .history
        .iter()
        .any(|line| line.plain().contains("88 tokens")));
}

#[test]
fn markdown_heading_renders_bold() {
    let lines = render_markdown("## Section title", usize::MAX);
    assert_eq!(markdown_text(&lines), vec!["Section title"]);
    assert!(lines[0]
        .spans
        .iter()
        .all(|span| span.style.add_modifier.contains(Modifier::BOLD)));
}

#[test]
fn markdown_bold_and_italic_render_inline() {
    let lines = render_markdown("a **bold** and *em* word", usize::MAX);
    assert_eq!(markdown_text(&lines), vec!["a bold and em word"]);
    let styled = |needle: &str| {
        lines[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == needle)
            .map(|span| span.style)
    };
    assert_eq!(styled("bold").unwrap().add_modifier, Modifier::BOLD);
    assert_eq!(styled("em").unwrap().add_modifier, Modifier::ITALIC);
}

#[test]
fn markdown_inline_code_recolors_and_restores_base() {
    let lines = render_markdown("run `a*b*c` now", usize::MAX);
    // Inline code is a chip (fg + subtle bg) as a single styled span.
    assert_eq!(markdown_text(&lines), vec!["run a*b*c now"]);
    let chip = lines[0]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "a*b*c")
        .expect("code span");
    assert_eq!(chip.style.bg, MD_CODE.bg);
    assert_eq!(chip.style.fg, MD_CODE.fg);
}

#[test]
fn markdown_fenced_code_block_renders_verbatim_with_gutter() {
    let md = "before\n```rust\nlet x = **2**;\nfoo();\n```\nafter";
    let lines = render_markdown(md, usize::MAX);
    assert_eq!(
        markdown_text(&lines),
        vec!["before", "│ let x = **2**;", "│ foo();", "after"]
    );
    // Code lines are dim; emphasis markers inside them stay literal.
    assert!(lines[1].spans.iter().all(|span| span.style.fg == MD_DIM.fg));
}

#[test]
fn markdown_unbalanced_markers_stay_literal() {
    let lines = render_markdown("2 * 3 = 6", usize::MAX);
    assert_eq!(markdown_text(&lines), vec!["2 * 3 = 6"]);
}

#[test]
fn markdown_table_renders_aligned_box() {
    let table = "| Name | Qty |\n|:-----|----:|\n| apple | 3 |\n| fig | 22 |";
    let lines = render_markdown(table, usize::MAX);
    let plain = markdown_text(&lines);

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
    // Header cells are bold.
    let header = lines[1]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "Name")
        .expect("header cell");
    assert!(header.style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn markdown_table_is_capped_to_width() {
    let table = "| A | B |\n|---|---|\n| xxxxxxxxxx | yyyyyyyyyy |";
    let lines = render_markdown(table, 20);
    for line in &lines {
        assert!(line.width() <= 20, "line exceeds width: {}", line);
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
        .any(|line| has_modifier(&line.line, Modifier::BOLD) && line.plain().contains("there")));
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
            cached_input_tokens: 0,
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
        .any(|line| line.plain().contains("42 tokens")));
    assert!(reasoning_entry(&app).is_some());
}

#[test]
fn collapsed_reasoning_message_keeps_only_duration_header() {
    let document = UiBuilder::new()
        .chat(&[ChatLine::Reasoning {
            text: "l1\nl2\nl3\nl4\nl5".to_string(),
            collapsed: true,
            duration_secs: Some(2),
        }])
        .finish();
    let visible: Vec<&UiLine> = document
        .history
        .iter()
        .filter(|line| !line.is_blank())
        .collect();
    assert_eq!(visible.len(), 1);
    assert!(visible[0].plain().contains("✻ thought for 2s"));
    // The header is the click target that expands the block again.
    assert_eq!(visible[0].click, Some(0));
}

#[test]
fn expanded_reasoning_message_renders_body_lines() {
    let document = UiBuilder::new()
        .chat(&[ChatLine::Reasoning {
            text: "l1\nl2".to_string(),
            collapsed: false,
            duration_secs: Some(90),
        }])
        .finish();
    let visible: Vec<String> = document
        .history
        .iter()
        .filter(|line| !line.is_blank())
        .map(|line| line.plain().trim().to_string())
        .collect();
    assert_eq!(visible, vec!["✻ thought for 1m 30s", "l1", "l2"]);
}

#[test]
fn compaction_events_set_progress_notices_and_context_meter() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.apply_events(vec![AgentEvent::ContextUsage {
        tokens: 900_000,
        tokenizer: "gpt-5".to_string(),
        cost: 0.0,
    }]);
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
        AgentEvent::ContextUsage {
            tokens: 25_000,
            tokenizer: "gpt-5".to_string(),
            cost: 0.0,
        },
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
    let preview = preview_text(&tool_output_preview("custom", &output, false));

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

    let visible_preview = preview_text(&tool_output_preview("str_replace", &output, false));

    assert!(visible_preview.contains("a (+1 -1)"));
    assert!(visible_preview.contains("1 -  old"));
    assert!(visible_preview.contains("1 +  new"));
    assert!(!visible_preview.contains("diff --git a/a b/a"));
    assert!(!visible_preview.contains("raw"));
}

#[test]
fn tool_output_preview_appends_rejected_hunk_count_after_partial_approval() {
    let output = serde_json::json!({
        "diff": "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n",
        "applied_hunks": ["f0h2"],
        "rejected_hunks": ["f0h1", "f0h3"],
    })
    .to_string();

    let visible_preview = preview_text(&tool_output_preview("str_replace", &output, false));

    // The diff shows only the applied hunks, plus a rejected-count line.
    assert!(visible_preview.contains("1 +  new"));
    assert!(visible_preview.contains("2 hunk(s) rejected by user (not applied)"));

    // A whole-call approval (nothing rejected) adds no such line.
    let full = serde_json::json!({
        "diff": "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n",
        "applied_hunks": ["f0h1"],
        "rejected_hunks": [],
    })
    .to_string();
    assert!(!preview_text(&tool_output_preview("str_replace", &full, false)).contains("rejected"));
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

    let preview = preview_text(&tool_output_preview("edit", &output, false));

    assert!(preview.contains("README.md (+2 -30)"));
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

    let read_preview = preview_text(&tool_output_preview("read", &read, false));
    let ls_preview = preview_text(&tool_output_preview("ls", &ls, false));

    assert_eq!(read_preview, "read main.rs: 12 lines from line 5");
    assert!(!read_preview.contains("hidden"));
    assert_eq!(ls_preview, "ls C:\\repo\\src");
}

#[test]
fn tool_error_json_shows_message_not_raw_json() {
    let preview = tool_output_preview(
        "write_stdin",
        r#"{"error":"shell session not found","session_id":0}"#,
        false,
    );
    assert_eq!(preview_text(&preview), "error: shell session not found");
    // The header names the real tool instead of a generic "Tool".
    let header = format_tool_header("write_stdin", false, preview.first().map(|l| &l.line), 60);
    assert!(header
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<String>()
        .contains("Sent"));
    // MCP tools show server/tool rather than the mcp__ prefix form.
    let header = format_tool_header("mcp__fs__read_file", false, None, 60);
    assert!(header
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<String>()
        .contains("fs/read_file"));
}

#[test]
fn running_session_preview_shows_session_not_exit_code() {
    let output = serde_json::json!({
        "command": "cargo test",
        "session_id": 3,
        "running": true,
        "stdout": "compiling…",
        "stderr": ""
    })
    .to_string();

    let preview = preview_text(&tool_output_preview("write_stdin", &output, false));
    assert!(preview.contains("cargo test  session 3 running"));
    assert!(!preview.contains("exit"));
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

    let plain_preview = preview_text(&tool_output_preview("bash", &output, false));

    assert!(plain_preview.contains("cargo test  exit 0"));
    assert!(plain_preview.contains("stdout:"));
    assert!(plain_preview.contains("three"));
}

#[test]
fn chat_history_separates_turns_with_blank_lines() {
    let document = UiBuilder::new()
        .chat(&[
            ChatLine::User("first".to_string()),
            ChatLine::Assistant("second".to_string()),
        ])
        .finish();

    let kinds: Vec<UiKind> = document.history.iter().map(|line| line.kind).collect();
    let blank = document
        .history
        .iter()
        .position(|line| line.is_blank())
        .expect("a blank line separates the two turns");
    assert_eq!(kinds[..blank], [UiKind::User]);
    assert_eq!(kinds[blank + 1..], [UiKind::Assistant]);
    // No leading blank before the first block.
    assert!(!document.history[0].is_blank());
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

    assert!(document
        .history
        .iter()
        .any(|line| line.kind == UiKind::DiffRemove && line.plain().contains("1 -  old")));
    assert!(document
        .history
        .iter()
        .any(|line| line.kind == UiKind::DiffAdd && line.plain().contains("1 +  new")));
}

#[test]
fn projection_only_indents_text_not_ui_elements() {
    let document = UiBuilder::new()
        .chat(&[
            ChatLine::Assistant("answer".to_string()),
            ChatLine::Tool {
                call_id: None,
                name: "edit".to_string(),
                output: serde_json::json!({
                    "diff": "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n"
                })
                .to_string(),
                running: false,
            },
        ])
        .input(&input_lines("hello"), &[], 0)
        .bottom_status(
            BottomStatus {
                provider: "jucode",
                model: "gpt-5",
                reasoning_effort: "medium",
                git: None,
                context_tokens: 10,
                context_window: 100,
                cost: 0.0,
            },
            40,
        )
        .finish();

    let frame = RenderedFrame::build(&document, 40);
    let plain = frame.lines.clone();

    assert!(plain.iter().any(|line| line.starts_with("  answer")));
    assert!(plain.iter().any(|line| line.is_empty()));
    assert!(plain.iter().any(|line| line.starts_with("● Edit")));
    assert!(plain.iter().any(|line| line.contains("⎿")));
    assert!(plain.iter().any(|line| line.contains("1 -")));
    assert!(plain.iter().any(|line| line.starts_with("› hello")));
    let bottom_status = plain
        .iter()
        .find(|line| line.contains("tokens 10/100"))
        .expect("bottom status should render");
    assert!(!bottom_status.starts_with("  "));
}

#[test]
fn projection_keeps_full_history_for_in_app_scroll() {
    let document = UiBuilder::new().finish_with_history_and_input(20);

    let frame = RenderedFrame::build(&document, 80);
    let output = frame.lines.join("\n");

    assert!(output.contains("line 0"));
    assert!(output.contains("line 19"));
    assert_eq!(frame.lines.len(), 24);
}

#[test]
fn assistant_delta_streams_into_transcript() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.apply_events(vec![
        AgentEvent::Connecting,
        AgentEvent::AssistantDelta("streaming".to_string()),
    ]);
    // The in-flight reply is already a transcript message, not a live layer.
    assert!(app
        .state
        .chat
        .iter()
        .any(|line| matches!(line, ChatLine::Assistant(text) if text == "streaming")));
    let document = app.build_document(80, Instant::now());
    let history = document
        .rendered_history_lines
        .expect("history lines should be projected");
    assert!(history
        .iter()
        .any(|line| line.plain().contains("streaming")));
    // Nothing leaks into the control region above the composer.
    assert!(!document
        .controls
        .iter()
        .any(|line| line.plain().contains("streaming")));
}

#[test]
fn approval_wait_does_not_extend_thinking_duration() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.apply_events(vec![
        AgentEvent::ThinkingStart,
        AgentEvent::ReasoningDelta("plan".to_string()),
        AgentEvent::ApprovalRequest {
            call_id: "c1".to_string(),
            name: "bash".to_string(),
            summary: "run it".to_string(),
            subagent_id: None,
            hunks: None,
        },
    ]);
    let index = app
        .state
        .chat
        .iter()
        .position(|line| matches!(line, ChatLine::Reasoning { .. }))
        .expect("reasoning block");
    // The reasoning block collapses when the approval prompt appears, so the
    // user's deliberation time is not counted as thinking.
    assert!(matches!(
        app.state.chat.get(index),
        Some(ChatLine::Reasoning {
            collapsed: true,
            duration_secs: Some(_),
            ..
        })
    ));
    let waited = app
        .state
        .chat
        .iter()
        .find_map(|line| match line {
            ChatLine::Reasoning { duration_secs, .. } => *duration_secs,
            _ => None,
        })
        .unwrap();
    assert!(waited < 5, "approval wait leaked into thinking time");
}

#[test]
fn cursor_row_accounts_for_full_history() {
    let document = UiBuilder::new().finish_with_history_and_input(20);

    let frame = RenderedFrame::build(&document, 80);
    let cursor = frame.cursor.expect("cursor marker should be found");

    assert_eq!(frame.lines.len(), 24);
    assert_eq!(cursor.row, 22);
    assert_eq!(cursor.column, 2);
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
        .any(|line| line.plain().contains("[-]") && line.plain().contains("user: root")));
    assert!(document
        .controls
        .iter()
        .any(|line| line.kind == UiKind::TreeDirectory
            && line.plain().contains("[+]")
            && line.plain().contains("user: child 2")));
}

#[test]
fn checkout_tree_marks_head_and_active_path() {
    // Linear history e1 -> e2 -> e3, with e3 as the current HEAD.
    let nodes = vec![
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
            active: true,
        },
    ];
    let tree = PickerState::checkout(nodes);
    let document = UiBuilder::new().picker(Some(&tree)).finish();
    let controls = &document.controls;

    // The HEAD node is annotated as the current position.
    assert!(controls
        .iter()
        .any(|line| line.plain().contains("user: third") && line.plain().contains("current")));
    // Every node on the path to the HEAD is bulleted (root e1 included).
    assert!(controls
        .iter()
        .any(|line| line.plain().contains('\u{2022}') && line.plain().contains("user: first")));
    // A position counter is shown; selection starts on the HEAD (row 3 of 3).
    assert!(controls.iter().any(|line| line.plain().contains("(3/3)")));
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
        .map(UiLine::plain)
        .collect::<Vec<_>>();

    assert!(controls.iter().any(|text| text.contains("thinking: none")));
    assert!(controls
        .iter()
        .any(|text| text.contains("gpt-5.2") && text.contains(" *")));
}

#[test]
fn login_paste_picker_submits_paste_command() {
    let mut picker = PickerState::login_paste();
    assert!(picker.prompt.is_some());
    for ch in "zcode://cb?code=abc&state=s".chars() {
        picker.push_prompt_char(ch);
    }
    assert_eq!(
        picker.take_prompt_command().as_deref(),
        Some("/login-paste zcode://cb?code=abc&state=s")
    );
    assert_eq!(picker.take_prompt_command(), None);
}

#[test]
fn shift_tab_effort_cycle_wraps() {
    let efforts = vec!["none".to_string(), "low".to_string(), "medium".to_string()];
    assert_eq!(next_reasoning_effort(&efforts, "none"), "low");
    assert_eq!(next_reasoning_effort(&efforts, "low"), "medium");
    assert_eq!(next_reasoning_effort(&efforts, "medium"), "none");
    assert_eq!(next_reasoning_effort(&efforts, "unknown"), "none");
}

#[test]
fn bang_input_runs_locally_and_never_reaches_the_model() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.input.push_text("!echo tui-local-shell");
    app.handle_key_at(KeyCode::Enter, KeyModifiers::empty(), Instant::now());

    assert!(app.runtime.submitted.is_empty(), "must not go to the model");
    assert!(app.runtime.commands.is_empty());
    assert!(app.input.text().is_empty());
    assert!(app
        .state
        .chat
        .iter()
        .any(|line| matches!(line, ChatLine::User(text) if text == "!echo tui-local-shell")));
    assert!(app.state.chat.iter().any(|line| matches!(
        line,
        ChatLine::Tool { name, running: true, .. } if name == "! echo tui-local-shell"
    )));

    let mut finished = false;
    for _ in 0..300 {
        for result in app.local_shell.poll() {
            app.state.finish_local_shell(result);
        }
        if app.state.chat.iter().any(|line| {
            matches!(
                line,
                ChatLine::Tool { output, running: false, .. } if output.contains("tui-local-shell")
            )
        }) {
            finished = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(finished, "local shell output never landed in history");
}

#[test]
fn lone_bang_submits_as_a_regular_message() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.input.push_text("!");
    app.handle_key_at(KeyCode::Enter, KeyModifiers::empty(), Instant::now());
    assert_eq!(app.runtime.submitted, vec!["!".to_string()]);
}

#[test]
fn at_mention_tab_completes_top_ranked_file() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.state.file_index = Some(vec![
        "docs/main-notes.md".to_string(),
        "src/main.rs".to_string(),
    ]);
    app.input.push_text("look at @main");

    app.handle_key_at(KeyCode::Tab, KeyModifiers::empty(), Instant::now());

    assert_eq!(app.input.text(), "look at @src/main.rs ");
    assert!(app.runtime.submitted.is_empty());
}

#[test]
fn at_mention_enter_completes_instead_of_submitting() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.state.file_index = Some(vec!["src/main.rs".to_string()]);
    app.input.push_text("@ma");

    app.handle_key_at(KeyCode::Enter, KeyModifiers::empty(), Instant::now());
    assert_eq!(app.input.text(), "@src/main.rs ");
    assert!(app.runtime.submitted.is_empty());

    // Second Enter submits the completed message.
    app.input.push_text("please review");
    app.handle_key_at(KeyCode::Enter, KeyModifiers::empty(), Instant::now());
    assert_eq!(
        app.runtime.submitted,
        vec!["@src/main.rs please review".to_string()]
    );
}

#[test]
fn at_mention_arrow_keys_cycle_candidates() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.state.file_index = Some(vec![
        "src/main.rs".to_string(),
        "src/maintenance.rs".to_string(),
    ]);
    app.input.push_text("@main");

    app.handle_key_at(KeyCode::Down, KeyModifiers::empty(), Instant::now());
    app.handle_key_at(KeyCode::Tab, KeyModifiers::empty(), Instant::now());

    assert_eq!(app.input.text(), "@src/maintenance.rs ");
}

#[test]
fn exact_mention_path_submits_on_enter() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.state.file_index = Some(vec!["src/main.rs".to_string()]);
    app.input.push_text("@src/main.rs");

    app.handle_key_at(KeyCode::Enter, KeyModifiers::empty(), Instant::now());

    assert_eq!(app.runtime.submitted, vec!["@src/main.rs".to_string()]);
}

#[test]
fn pasted_image_path_is_attached_not_inserted() {
    let dir = std::env::temp_dir().join(format!(
        "jucode-tui-image-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("shot.png");
    std::fs::write(&path, b"fake png bytes").unwrap();

    let mut app = TuiApp::new(TestRuntime::default());
    app.handle_paste(&path.display().to_string());
    assert_eq!(
        app.runtime.commands,
        vec![format!("/image {}", path.display())]
    );
    assert!(app.input.text().is_empty());

    // Quoted drag-and-drop form also resolves.
    app.handle_paste(&format!("\"{}\"", path.display()));
    assert_eq!(app.runtime.commands.len(), 2);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn pasted_non_image_text_stays_in_input() {
    let mut app = TuiApp::new(TestRuntime::default());
    app.handle_paste("notes.txt and some text");
    assert_eq!(app.input.text(), "notes.txt and some text");
    assert!(app.runtime.commands.is_empty());
}

#[test]
fn bottom_status_shows_git_branch_with_dirty_marker() {
    let dirty = crate::git_bar::GitStatus {
        branch: "main".to_string(),
        dirty: true,
    };
    let document = UiBuilder::new()
        .bottom_status(
            BottomStatus {
                provider: "p",
                model: "m",
                reasoning_effort: "low",
                git: Some(&dirty),
                context_tokens: 1,
                context_window: 100,
                cost: 0.0,
            },
            64,
        )
        .finish();
    let line = document.controls.last().unwrap().plain();
    assert!(line.starts_with("p / m (low) main*"), "{line}");
}

trait TestUiBuilderExt {
    fn finish_with_history_and_input(self, history_lines: usize) -> UiDocument;
}

impl TestUiBuilderExt for UiBuilder {
    fn finish_with_history_and_input(mut self, history_lines: usize) -> UiDocument {
        for index in 0..history_lines {
            self.history_line(UiKind::Assistant, format!("line {index}"));
        }
        self.input(&input_lines(""), &[], 0).finish()
    }
}
