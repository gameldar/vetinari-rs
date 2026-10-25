//! Drawing a frame from the state.
//!
//! One pane, sized to sit in a corner of a larger layout: the queue with
//! what recently happened under it, then a line that is the status when
//! there is one and the key hints otherwise. The details, which-key and
//! other overlays are drawn over it.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use vetinari_core::application::queue::Lane;
use vetinari_core::domain::attention::ItemState;
use vetinari_core::domain::devflow::lanes::RowKind;
use vetinari_core::domain::devflow::step::GateKind;

use crate::app::{App, Level};
use crate::overlay::{Form, Menu, Overlay, Prompt, RunView, TextView};

/// Draw the whole frame.
pub fn render(frame: &mut Frame, app: &App) {
    let [body, bottom] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());

    render_queue(frame, app, body);
    render_bottom(frame, app, bottom);

    if let Some(which_key) = app.which_key() {
        render_which_key(frame, which_key, body);
    }
    if let Some(overlay) = app.overlay() {
        render_overlay(frame, app, overlay, frame.area());
    }
}

fn render_queue(frame: &mut Frame, app: &App, area: Rect) {
    let style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(Span::styled(format!(" {} ", app.title()), style));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let recent = app.recent_actions();
    let recent_height = if recent.is_empty() {
        0
    } else {
        u16::try_from(recent.len() + 1).unwrap_or(u16::MAX)
    };
    let [rows_area, recent_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(recent_height)]).areas(inner);

    render_rows(frame, app, rows_area);
    render_recent(frame, &recent, recent_area);
}

fn render_rows(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.rows();
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("Nothing is waiting on you. Press r to refresh.")
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let cursor = app.cursor();
    let mut lines: Vec<ListItem> = Vec::new();
    let mut selected_line = 0;
    let mut lane = None;
    for (index, row) in rows.iter().enumerate() {
        if lane != Some(row.lane) {
            lane = Some(row.lane);
            lines.push(ListItem::new(Line::styled(
                row.lane.title(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        let item = row.entry.item();
        let selected = index == cursor;
        if selected {
            selected_line = lines.len();
        }
        let marker = if selected { "> " } else { "  " };
        let relative = item
            .relative_time(app.now())
            .map(|when| format!(" [{when}]"))
            .unwrap_or_default();
        let note = app
            .row_note(row)
            .map(|note| format!(" - {note}"))
            .unwrap_or_default();
        let style = match (row.lane, item.state()) {
            (Lane::Done | Lane::Quiet, _) => Style::default().fg(Color::DarkGray),
            (_, ItemState::AgentWaiting | ItemState::Blocked) => {
                Style::default().fg(Color::Magenta)
            }
            (_, ItemState::CiFailed | ItemState::AgentFailed) => Style::default().fg(Color::Red),
            _ => Style::default(),
        };
        let style = if selected {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        };
        lines.push(ListItem::new(Line::styled(
            format!("{marker}{}{relative}{note}", item.row_label()),
            style,
        )));
    }

    let mut state = ListState::default().with_selected(Some(selected_line));
    frame.render_stateful_widget(List::new(lines), area, &mut state);
}

fn render_recent(frame: &mut Frame, recent: &[String], area: Rect) {
    if recent.is_empty() {
        return;
    }
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = vec![Line::styled("RECENT", dim.add_modifier(Modifier::BOLD))];
    lines.extend(
        recent
            .iter()
            .map(|action| Line::styled(action.clone(), dim)),
    );
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_details(frame: &mut Frame, app: &App, area: Rect) {
    let details = app.details();
    let lines: Vec<Line> = details
        .iter()
        .map(|(key, value)| {
            Line::from(vec![
                Span::styled(format!("{key}: "), Style::default().fg(Color::DarkGray)),
                Span::raw(value.clone()),
            ])
        })
        .collect();
    let height = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let popup = centred(area, 90, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Details "))
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn render_bottom(frame: &mut Frame, app: &App, area: Rect) {
    let line = match app.status() {
        Some(message) => {
            let style = match message.level {
                Level::Info => Style::default().fg(Color::Green),
                Level::Warn => Style::default().fg(Color::Yellow),
                Level::Error => Style::default().fg(Color::Red),
            };
            Line::styled(message.text.clone(), style)
        }
        None => Line::styled(app.footer(), Style::default().fg(Color::DarkGray)),
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_which_key(frame: &mut Frame, which_key: &crate::app::WhichKeyView, area: Rect) {
    let height = u16::try_from(which_key.entries.len().min(8) + 2).unwrap_or(4);
    let [_, popup] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(height)]).areas(area);
    let entries: Vec<ListItem> = which_key
        .entries
        .iter()
        .take(8)
        .map(|(key, label)| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{key:<6}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(label.clone()),
            ]))
        })
        .collect();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        List::new(entries).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", which_key.prefix)),
        ),
        popup,
    );
}

fn centred(area: Rect, width_percent: u16, height: u16) -> Rect {
    let [horizontal] = Layout::horizontal([Constraint::Percentage(width_percent)])
        .flex(Flex::Center)
        .areas(area);
    let [vertical] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(horizontal);
    vertical
}

fn render_overlay(frame: &mut Frame, app: &App, overlay: &Overlay, area: Rect) {
    match overlay {
        Overlay::Details => render_details(frame, app, area),
        Overlay::Prompt(prompt) => render_prompt(frame, prompt, area),
        Overlay::Menu(menu) => render_menu(frame, menu, area),
        Overlay::Form(form) => render_form(frame, form, area),
        Overlay::Text(view) => render_text(frame, view, area),
        Overlay::Run(view) => render_run(frame, view, area),
        Overlay::DeviceCode(view) => {
            let popup = centred(area, 70, 9);
            let lines = vec![
                Line::raw(format!("Authorising {}", view.provider.display_name())),
                Line::raw(""),
                Line::from(vec![
                    Span::raw("Visit  "),
                    Span::styled(
                        view.challenge.verification_uri.clone(),
                        Style::default().fg(Color::Cyan),
                    ),
                ]),
                Line::from(vec![
                    Span::raw("Code   "),
                    Span::styled(
                        view.challenge.user_code.clone(),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::raw(""),
                Line::styled(view.message.clone(), Style::default().fg(Color::DarkGray)),
            ];
            frame.render_widget(Clear, popup);
            frame.render_widget(
                Paragraph::new(lines)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" Device authorisation "),
                    )
                    .wrap(Wrap { trim: true }),
                popup,
            );
        }
    }
}

fn render_prompt(frame: &mut Frame, prompt: &Prompt, area: Rect) {
    let height = if prompt.input.is_multiline() { 12 } else { 6 };
    let popup = centred(area, 70, height);
    let mut lines = vec![Line::styled(
        prompt.hint.clone(),
        Style::default().fg(Color::DarkGray),
    )];
    lines.push(Line::raw(""));
    for (index, line) in prompt.input.value().split('\n').enumerate() {
        let shown = if index == 0 {
            format!("> {line}")
        } else {
            format!("  {line}")
        };
        lines.push(Line::raw(shown));
    }
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", prompt.title)),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn render_menu(frame: &mut Frame, menu: &Menu, area: Rect) {
    let height = u16::try_from(menu.items.len().min(12) + 2).unwrap_or(4);
    let popup = centred(area, 60, height.max(3));
    let items: Vec<ListItem> = menu
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let selected = index == menu.cursor();
            let marker = if selected { "> " } else { "  " };
            let style = if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker}{}", item.label), style),
                Span::styled(
                    if item.detail.is_empty() {
                        String::new()
                    } else {
                        format!("  {}", item.detail)
                    },
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", menu.title)),
        ),
        popup,
    );
}

fn render_form(frame: &mut Frame, form: &Form, area: Rect) {
    let popup = centred(area, 80, 20);
    let mut lines = Vec::new();
    for (index, field) in form.fields.iter().enumerate() {
        let focused = index == form.cursor();
        let marker = if focused { ">" } else { " " };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{marker} {:<26}", field.label),
                if focused {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::raw(field.input.value().replace('\n', " / ")),
        ]));
    }
    if !form.problems.is_empty() {
        lines.push(Line::raw(""));
        for problem in &form.problems {
            lines.push(Line::styled(
                format!("! {problem}"),
                Style::default().fg(Color::Red),
            ));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "TAB next field - C-s save - ESC cancel",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Skill editor "),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

/// A run's popup (design 9.3): a row per node, indented under its loop,
/// the latest questions and findings, and the keys the open gate takes.
fn render_run(frame: &mut Frame, view: &RunView, area: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = Vec::new();
    match &view.detail {
        None => lines.push(Line::styled("waiting for vetinarid...", dim)),
        Some(detail) => {
            lines.push(Line::raw(detail.item.summary.strip()));
            lines.push(Line::raw(""));
            for row in &detail.rows {
                let mut text = format!(
                    "{}{} {}",
                    "  ".repeat(row.row.indent),
                    row.mark.glyph(),
                    row.row.node
                );
                if let RowKind::Loop {
                    selected, bound, ..
                } = row.row.kind
                {
                    text.push_str(&format!("  < {selected}/{bound} >"));
                }
                if let Some(attempt) = row.row.state.as_ref().and_then(|state| state.attempt)
                    && attempt.get() > 1
                {
                    text.push_str(&format!("  attempt {attempt}"));
                }
                lines.push(Line::raw(text));
            }
            if !detail.notes.questions.is_empty() {
                lines.push(Line::raw(""));
                lines.push(Line::styled("Preflight asks:", dim));
                for question in &detail.notes.questions {
                    lines.push(Line::raw(format!("  - {}", question.as_str())));
                }
            }
            if let Some(review) = &detail.notes.findings {
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    format!("Review round {} found:", review.round),
                    dim,
                ));
                for finding in &review.findings {
                    let place = match (&finding.file, finding.line) {
                        (Some(file), Some(line)) => format!("{}:{line}  ", file.as_str()),
                        (Some(file), None) => format!("{}  ", file.as_str()),
                        _ => String::new(),
                    };
                    lines.push(Line::raw(format!("  - {place}{}", finding.text.as_str())));
                }
            }
        }
    }
    let gate_keys = match view.gate() {
        Some((_, GateKind::InfoNeeded)) => "r recheck  n recheck with a note  ",
        Some((_, GateKind::ReviewStuck)) => "o open the PR anyway  ",
        None => "",
    };
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("{gate_keys}x abandon  </> round  ESC close"),
        dim,
    ));
    let height = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let popup = centred(area, 80, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", view.title)),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn render_text(frame: &mut Frame, view: &TextView, area: Rect) {
    let popup = centred(area, 80, 20);
    let lines: Vec<Line> = view
        .lines
        .iter()
        .skip(view.scroll)
        .map(|line| Line::raw(line.clone()))
        .collect();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", view.title))
                    .title_alignment(Alignment::Left),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use std::path::PathBuf;
    use time::macros::datetime;
    use time::{Duration, OffsetDateTime};
    use vetinari_core::domain::attention::{
        AttentionItem, ItemContext, ItemState, PullRequestRef, SourceCategory,
    };
    use vetinari_core::domain::feedback::FeedbackTarget;
    use vetinari_core::domain::ids::SourceId;
    use vetinari_core::domain::keymap::Keymap;
    use vetinari_core::domain::keys::{KeyChord, KeyCode};
    use vetinari_core::domain::profile::ProfileSet;
    use vetinari_core::domain::queue::rules::QueueRules;
    use vetinari_core::domain::skill::SkillLibrary;
    use vetinari_core::domain::text::NonEmptyText;
    use vetinari_core::testing::polled;

    use crate::app::{StatusMessage, Update};

    const NOW: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

    fn text_of(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn drawn(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|frame| render(frame, app)).expect("draw");
        text_of(terminal.backend().buffer())
    }

    fn pr_item() -> AttentionItem {
        AttentionItem::builder(
            &SourceId::parse("github").expect("id"),
            "acme/web#142",
            SourceCategory::CodeReview,
            NonEmptyText::parse("Fix memory leak").expect("title"),
        )
        .state(ItemState::CiFailed)
        .author("developer")
        .occurred_at(NOW)
        .context(ItemContext::PullRequest(PullRequestRef {
            repo: "acme/web".to_owned(),
            number: 142,
            head_ref: "fix/leak".to_owned(),
            base_ref: "main".to_owned(),
            clone_url: "u".to_owned(),
        }))
        .build()
    }

    fn event_item() -> AttentionItem {
        AttentionItem::builder(
            &SourceId::parse("calendar").expect("id"),
            "evt-1",
            SourceCategory::Calendar,
            NonEmptyText::parse("Sprint planning").expect("title"),
        )
        .state(ItemState::Scheduled {
            starts_at: NOW + Duration::minutes(15),
        })
        .occurred_at(NOW)
        .build()
    }

    fn app() -> App {
        let mut app = App::new(
            Keymap::doom_defaults(),
            ProfileSet::defaults(),
            SkillLibrary::builtin(),
            FeedbackTarget::new(
                NonEmptyText::parse("vetinari").expect("name"),
                PathBuf::from("/repo"),
            ),
            QueueRules::default(),
            50,
            NOW,
        );
        app.apply(Update::Polled(polled(vec![pr_item(), event_item()])));
        app.apply(Update::Keychain("Keychain: 2 authenticated".to_owned()));
        app
    }

    fn press(app: &mut App, spelling: &str) {
        for part in spelling.split_whitespace() {
            app.on_key(KeyChord::parse(part).expect("chord"));
        }
    }

    #[test]
    fn the_queue_is_titled_with_the_profile() {
        let rendered = drawn(&app(), 100, 24);
        assert!(rendered.contains("Vetinari · Work"), "{rendered}");
    }

    #[test]
    fn the_queue_is_the_only_pane() {
        let rendered = drawn(&app(), 100, 24);
        assert!(!rendered.contains("WORKSPACE"), "{rendered}");
        assert!(!rendered.contains("CONTEXT / DETAILS"), "{rendered}");
    }

    #[test]
    fn feed_rows_are_drawn_with_their_category_and_state() {
        let rendered = drawn(&app(), 100, 24);
        assert!(rendered.contains("[CAL] Sprint planning (scheduled)"));
        assert!(rendered.contains("[PR] Fix memory leak (CI failed)"));
        assert!(rendered.contains("in 15m"), "a meeting says how soon it is");
    }

    #[test]
    fn the_queue_title_counts_each_lane() {
        let mut app = app();
        press(&mut app, "SPC n");

        let rendered = drawn(&app, 100, 24);

        assert!(rendered.contains("(now 1, next 1)"), "got {rendered}");
    }

    #[test]
    fn the_queue_is_drawn_lane_by_lane() {
        let mut app = app();
        press(&mut app, "SPC n");

        let rendered = drawn(&app, 100, 24);

        let now = rendered.find("NOW").expect("a NOW heading");
        let meeting = rendered.find("Sprint planning").expect("the meeting");
        let next = rendered.find("NEXT").expect("a NEXT heading");
        let pr = rendered.find("Fix memory leak").expect("the PR");
        assert!(now < meeting && meeting < next && next < pr, "{rendered}");
    }

    #[test]
    fn a_parked_row_says_when_it_comes_back() {
        let mut app = app();
        press(&mut app, "SPC d p h");

        let rendered = drawn(&app, 200, 24);

        assert!(rendered.contains("back in 1h"), "{rendered}");
    }

    #[test]
    fn the_selected_row_is_marked() {
        let rendered = drawn(&app(), 100, 24);
        assert!(
            rendered.contains("> [CAL] Sprint planning"),
            "the most urgent row starts selected"
        );
    }

    #[test]
    fn an_empty_queue_says_what_to_do_rather_than_showing_a_blank_box() {
        let app = App::new(
            Keymap::doom_defaults(),
            ProfileSet::defaults(),
            SkillLibrary::builtin(),
            FeedbackTarget::new(
                NonEmptyText::parse("vetinari").expect("name"),
                PathBuf::from("/repo"),
            ),
            QueueRules::default(),
            50,
            NOW,
        );
        let rendered = drawn(&app, 100, 24);
        assert!(rendered.contains("Nothing is waiting on you"));
    }

    #[test]
    fn the_filter_is_shown_in_the_feed_title_while_it_is_in_force() {
        let mut app = app();
        press(&mut app, "/");
        for ch in "memory".chars() {
            app.on_key(KeyChord::plain(KeyCode::Char(ch)));
        }
        press(&mut app, "RET");
        let rendered = drawn(&app, 100, 24);
        assert!(rendered.contains("/memory"));
    }

    #[test]
    fn return_shows_the_details_of_the_selected_row_over_the_queue() {
        let mut app = app();
        press(&mut app, "j");
        press(&mut app, "RET");

        let rendered = drawn(&app, 100, 30);

        assert!(rendered.contains("Title: Fix memory leak"), "{rendered}");
        assert!(rendered.contains("Author: developer"), "{rendered}");
    }

    #[test]
    fn the_bottom_line_lists_the_headline_bindings() {
        let rendered = drawn(&app(), 100, 24);
        assert!(rendered.contains("SPC n: Next"), "{rendered}");
        assert!(rendered.contains("RET: Details"), "{rendered}");
    }

    #[test]
    fn a_status_message_takes_the_place_of_the_key_hints() {
        let mut app = app();
        app.apply(Update::Status(StatusMessage::error("jira is unavailable")));
        let rendered = drawn(&app, 100, 24);
        assert!(rendered.contains("jira is unavailable"));
        assert!(!rendered.contains("SPC n: Next"), "{rendered}");
    }

    #[test]
    fn recent_actions_are_listed_under_the_queue() {
        let mut app = app();
        press(&mut app, "j");
        press(&mut app, "SPC d p h");

        let rendered = drawn(&app, 100, 24);

        let row = rendered.find("[PR] Fix memory leak").expect("the row");
        let recent = rendered.find("RECENT").expect("a recent heading");
        let action = rendered
            .find("09:00 parked: Fix memory leak")
            .expect("the action");
        assert!(row < recent && recent < action, "{rendered}");
    }

    #[test]
    fn with_nothing_done_yet_there_is_no_recent_section() {
        let rendered = drawn(&app(), 100, 24);
        assert!(!rendered.contains("RECENT"), "{rendered}");
    }

    #[test]
    fn the_selected_row_stays_in_view_in_a_short_panel() {
        let mut app = app();
        press(&mut app, "G");

        let rendered = drawn(&app, 60, 5);

        assert!(rendered.contains("> [PR] Fix memory leak"), "{rendered}");
    }

    #[test]
    fn a_panel_the_size_of_a_zax_section_shows_the_queue_and_what_just_happened() {
        let mut app = app();
        press(&mut app, "SPC n");

        let rendered = drawn(&app, 60, 12);

        assert!(rendered.contains("> [CAL] Sprint planning"), "{rendered}");
        assert!(
            rendered.contains("09:00 taken: Sprint planning"),
            "{rendered}"
        );
    }

    #[test]
    fn a_pending_prefix_draws_the_which_key_popup() {
        let mut app = app();
        press(&mut app, "SPC");
        let rendered = drawn(&app, 100, 24);
        assert!(rendered.contains("SPC"));
        assert!(rendered.contains("+feedback") || rendered.contains("+agents"));
    }

    #[test]
    fn the_feedback_overlay_is_drawn_over_the_body_with_what_was_typed() {
        let mut app = app();
        press(&mut app, "SPC f f");
        for ch in "add group labels".chars() {
            app.on_key(KeyChord::plain(KeyCode::Char(ch)));
        }
        let rendered = drawn(&app, 100, 30);
        assert!(rendered.contains("Feedback - vetinari"));
        assert!(rendered.contains("add group labels"));
        assert!(rendered.contains("C-s submits"));
    }

    #[test]
    fn a_menu_overlay_marks_the_highlighted_row() {
        let mut app = app();
        press(&mut app, "SPC p l");
        let rendered = drawn(&app, 100, 30);
        assert!(rendered.contains("Context profile"));
        assert!(rendered.contains("> Work"));
        assert!(rendered.contains("Home"));
    }

    #[test]
    fn the_skill_editor_shows_its_fields_and_its_problems() {
        let mut app = app();
        press(&mut app, "SPC s e");
        press(&mut app, "RET");
        press(&mut app, "C-s");
        let rendered = drawn(&app, 110, 30);
        assert!(rendered.contains("Skill editor"));
        assert!(rendered.contains("Prompt template"));
        assert!(rendered.contains("! id:"));
        assert!(rendered.contains("TAB next field"));
    }

    #[test]
    fn the_device_code_overlay_shows_the_code_and_where_to_type_it() {
        let mut app = app();
        app.apply(Update::DeviceCode(
            vetinari_core::domain::credentials::Provider::GoogleCalendar,
            vetinari_core::application::ports::DeviceCodeChallenge {
                user_code: "WXYZ-1234".to_owned(),
                verification_uri: "https://google.test/device".to_owned(),
                device_code: "dev".to_owned(),
                interval_secs: 5,
                expires_at: NOW + Duration::minutes(10),
            },
        ));
        let rendered = drawn(&app, 100, 30);
        assert!(rendered.contains("WXYZ-1234"));
        assert!(rendered.contains("https://google.test/device"));
        assert!(rendered.contains("Google Calendar"));
    }

    #[test]
    fn agent_output_is_drawn_in_a_scrollable_overlay() {
        let mut app = app();
        app.apply(Update::AgentOutput(
            vetinari_core::domain::ids::SessionId::parse("s1").expect("id"),
            vec!["running tests".to_owned(), "all green".to_owned()],
        ));
        let rendered = drawn(&app, 100, 30);
        assert!(rendered.contains("Agent output"));
        assert!(rendered.contains("running tests"));

        press(&mut app, "j");
        let rendered = drawn(&app, 100, 30);
        assert!(!rendered.contains("running tests"), "it scrolled");
        assert!(rendered.contains("all green"));
    }

    #[test]
    fn help_is_drawn_as_a_list_of_bindings() {
        let mut app = app();
        press(&mut app, "?");
        let rendered = drawn(&app, 100, 30);
        assert!(rendered.contains("Keybindings"));
        assert!(rendered.contains("SPC f f"));
    }

    #[test]
    fn a_small_terminal_is_drawn_without_panicking() {
        let mut app = app();
        for size in [(40_u16, 10_u16), (20, 6), (8, 3)] {
            let rendered = drawn(&app, size.0, size.1);
            assert!(!rendered.is_empty());
        }
        press(&mut app, "SPC f f");
        for size in [(40_u16, 10_u16), (20, 6), (8, 3)] {
            let _ = drawn(&app, size.0, size.1);
        }
    }

    mod runs {
        use super::*;
        use crate::fixtures::{detail, item};

        fn with_a_run() -> App {
            let mut app = app();
            let source = SourceId::parse("vetinarid").expect("source");
            app.apply(Update::Polled(vec![
                vetinari_core::application::attention::Polled {
                    source: source.clone(),
                    outcome: Ok(vec![item().to_attention(&source, NOW)]),
                },
            ]));
            app
        }

        #[test]
        fn a_run_is_queued_with_its_phase_strip() {
            let rendered = drawn(&with_a_run(), 120, 24);
            assert!(
                rendered.contains("[run] ENG-398 Paginate audit export  pre?"),
                "{rendered}"
            );
        }

        #[test]
        fn the_popup_shows_the_rows_the_questions_and_the_gate_keys() {
            let mut app = with_a_run();
            press(&mut app, "g g");
            while !app
                .selected_item()
                .is_some_and(|item| item.title().as_str().starts_with("ENG-398"))
            {
                press(&mut app, "j");
            }
            press(&mut app, "RET");
            app.apply(Update::RunDetail(Box::new(detail())));
            let rendered = drawn(&app, 120, 30);
            assert!(
                rendered.contains("ENG-398 Paginate audit export"),
                "{rendered}"
            );
            assert!(rendered.contains("? intake  < 2/3 >"), "{rendered}");
            assert!(rendered.contains("  ? ask-info"), "{rendered}");
            assert!(rendered.contains("Page size?"), "{rendered}");
            assert!(rendered.contains("r recheck"), "{rendered}");
            assert!(rendered.contains("n recheck with a note"), "{rendered}");
        }

        #[test]
        fn the_popup_waits_for_the_daemon_before_its_first_detail() {
            let mut app = with_a_run();
            press(&mut app, "g g");
            while !app
                .selected_item()
                .is_some_and(|item| item.title().as_str().starts_with("ENG-398"))
            {
                press(&mut app, "j");
            }
            press(&mut app, "RET");
            let rendered = drawn(&app, 120, 30);
            assert!(rendered.contains("waiting for vetinarid"), "{rendered}");
        }
    }
}
