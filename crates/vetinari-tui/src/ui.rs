//! Drawing a frame from the state.
//!
//! The layout follows the PRD: a header, the feed beside the workspace and
//! agent monitor with the details pane under it, then the status line and the
//! key hints. Overlays and the which-key popup are drawn over the body.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::app::{App, Level, Pane};
use crate::overlay::{Form, Menu, Overlay, Prompt, TextView};

/// Draw the whole frame.
pub fn render(frame: &mut Frame, app: &App) {
    let [header, body, status, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_header(frame, app, header);
    render_body(frame, app, body);
    render_status(frame, app, status);
    render_footer(frame, app, footer);

    if let Some(which_key) = app.which_key() {
        render_which_key(frame, which_key, body);
    }
    if let Some(overlay) = app.overlay() {
        render_overlay(frame, overlay, frame.area());
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let left = Span::styled(
        app.header_left(),
        Style::default().fg(Color::Black).bg(Color::Cyan),
    );
    let right = Span::styled(
        app.header_right(),
        Style::default().fg(Color::Black).bg(Color::Cyan),
    );
    let [left_area, right_area] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(u16::try_from(app.header_right().chars().count()).unwrap_or(0)),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(left)).style(Style::default().bg(Color::Cyan)),
        left_area,
    );
    frame.render_widget(Paragraph::new(Line::from(right)), right_area);
}

fn render_body(frame: &mut Frame, app: &App, area: Rect) {
    let [feed_area, right] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(area);
    let [monitor_area, details_area] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(right);

    render_feed(frame, app, feed_area);
    render_monitor(frame, app, monitor_area);
    render_details(frame, app, details_area);
}

fn pane_block(app: &App, pane: Pane, extra: &str) -> Block<'static> {
    let focused = app.focus() == pane;
    let title = if extra.is_empty() {
        pane.title().to_owned()
    } else {
        format!("{} {extra}", pane.title())
    };
    let style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(Span::styled(
            if focused {
                format!("> {title}")
            } else {
                format!("  {title}")
            },
            style,
        ))
}

fn render_feed(frame: &mut Frame, app: &App, area: Rect) {
    let visible = app.feed().visible();
    let extra = if app.feed().filter().is_empty() {
        let counts = app.feed().feed().counts_by_category();
        if counts.is_empty() {
            "(0)".to_owned()
        } else {
            let summary = counts
                .iter()
                .map(|(category, count)| format!("{} {count}", category.label()))
                .collect::<Vec<_>>()
                .join(" ");
            format!("({summary})")
        }
    } else {
        format!("(/{} - {})", app.feed().filter(), visible.len())
    };
    let block = pane_block(app, Pane::Feed, &extra);

    if visible.is_empty() {
        frame.render_widget(
            Paragraph::new("Nothing is waiting on you. Press r to refresh.")
                .block(block)
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let rows: Vec<ListItem> = visible
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let selected = index == app.feed().cursor();
            let marker = if selected { "> " } else { "  " };
            let relative = item
                .relative_time(app.now())
                .map(|when| format!(" [{when}]"))
                .unwrap_or_default();
            let style = match item.state() {
                vetinari_core::domain::attention::ItemState::AgentWaiting
                | vetinari_core::domain::attention::ItemState::Blocked => {
                    Style::default().fg(Color::Magenta)
                }
                vetinari_core::domain::attention::ItemState::CiFailed
                | vetinari_core::domain::attention::ItemState::AgentFailed => {
                    Style::default().fg(Color::Red)
                }
                vetinari_core::domain::attention::ItemState::AgentDone
                | vetinari_core::domain::attention::ItemState::Resolved => {
                    Style::default().fg(Color::DarkGray)
                }
                _ => Style::default(),
            };
            let style = if selected {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            };
            ListItem::new(Line::styled(
                format!("{marker}{}{relative}", item.row_label()),
                style,
            ))
        })
        .collect();

    frame.render_widget(List::new(rows).block(block), area);
}

fn render_monitor(frame: &mut Frame, app: &App, area: Rect) {
    let block = pane_block(app, Pane::Monitor, "");
    let mut rows: Vec<ListItem> = app
        .sessions()
        .iter()
        .enumerate()
        .map(|(index, session)| {
            let selected = app.focus() == Pane::Monitor && index == app.monitor_cursor();
            let marker = if selected { "> " } else { "  " };
            let style = if session.blocked {
                Style::default().fg(Color::Magenta)
            } else {
                Style::default()
            };
            ListItem::new(Line::styled(format!("{marker}{}", session.label()), style))
        })
        .collect();

    if !app.observed_agents().is_empty() {
        rows.push(ListItem::new(Line::styled(
            "  -- agents Vetinari did not start --",
            Style::default().fg(Color::DarkGray),
        )));
        rows.extend(app.observed_agents().iter().map(|agent| {
            let style = if agent.blocked {
                Style::default().fg(Color::Magenta)
            } else {
                Style::default().fg(Color::Cyan)
            };
            ListItem::new(Line::styled(format!("  {}", agent.label), style))
        }));
    }

    if !app.spaces().is_empty() {
        rows.push(ListItem::new(Line::styled(
            "  -- spaces --",
            Style::default().fg(Color::DarkGray),
        )));
        rows.extend(app.spaces().iter().map(|space| {
            ListItem::new(Line::styled(
                format!("  {}", space.label),
                Style::default().fg(Color::Blue),
            ))
        }));
    }

    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("No agents running. SPC p r reviews a PR, SPC f f improves Vetinari.")
                .block(block)
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    frame.render_widget(List::new(rows).block(block), area);
}

fn render_details(frame: &mut Frame, app: &App, area: Rect) {
    let block = pane_block(app, Pane::Details, "");
    let details = app.details();
    if details.is_empty() {
        frame.render_widget(Paragraph::new("").block(block), area);
        return;
    }
    let lines: Vec<Line> = details
        .iter()
        .map(|(key, value)| {
            Line::from(vec![
                Span::styled(format!("{key}: "), Style::default().fg(Color::DarkGray)),
                Span::raw(value.clone()),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let Some(message) = app.status() else {
        return;
    };
    let style = match message.level {
        Level::Info => Style::default().fg(Color::Green),
        Level::Warn => Style::default().fg(Color::Yellow),
        Level::Error => Style::default().fg(Color::Red),
    };
    frame.render_widget(
        Paragraph::new(Line::styled(message.text.clone(), style)),
        area,
    );
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::styled(
            app.footer(),
            Style::default().fg(Color::DarkGray),
        )),
        area,
    );
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

fn render_overlay(frame: &mut Frame, overlay: &Overlay, area: Rect) {
    match overlay {
        Overlay::Prompt(prompt) => render_prompt(frame, prompt, area),
        Overlay::Menu(menu) => render_menu(frame, menu, area),
        Overlay::Form(form) => render_form(frame, form, area),
        Overlay::Text(view) => render_text(frame, view, area),
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
    use vetinari_core::application::attention::Feed;
    use vetinari_core::domain::attention::{
        AttentionItem, ItemContext, ItemState, PullRequestRef, SourceCategory,
    };
    use vetinari_core::domain::feedback::FeedbackTarget;
    use vetinari_core::domain::ids::SourceId;
    use vetinari_core::domain::keymap::Keymap;
    use vetinari_core::domain::keys::{KeyChord, KeyCode};
    use vetinari_core::domain::profile::ProfileSet;
    use vetinari_core::domain::skill::SkillLibrary;
    use vetinari_core::domain::text::NonEmptyText;

    use crate::app::{ObservedAgentRow, SessionRow, SpaceRow, StatusMessage, Update};

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
            50,
            NOW,
        );
        app.apply(Update::Feed(Feed::new(
            vec![pr_item(), event_item()],
            Vec::new(),
            NOW,
        )));
        app.apply(Update::Keychain("Keychain: 2 authenticated".to_owned()));
        app
    }

    fn press(app: &mut App, spelling: &str) {
        for part in spelling.split_whitespace() {
            app.on_key(KeyChord::parse(part).expect("chord"));
        }
    }

    #[test]
    fn the_header_shows_the_profile_and_the_keychain() {
        let rendered = drawn(&app(), 100, 24);
        assert!(rendered.contains("VETINARI"));
        assert!(rendered.contains("Profile: Work"));
        assert!(rendered.contains("Keychain: 2 authenticated"));
    }

    #[test]
    fn the_three_panes_are_all_titled() {
        let rendered = drawn(&app(), 100, 24);
        assert!(rendered.contains("ATTENTION FEED"));
        assert!(rendered.contains("WORKSPACE & AGENT MONITOR"));
        assert!(rendered.contains("CONTEXT / DETAILS"));
    }

    #[test]
    fn the_focused_pane_is_marked_so_the_developer_knows_where_keys_go() {
        let mut app = app();
        let rendered = drawn(&app, 100, 24);
        assert!(rendered.contains("> ATTENTION FEED"));

        press(&mut app, "l");
        let rendered = drawn(&app, 100, 24);
        assert!(rendered.contains("> WORKSPACE"));
        assert!(!rendered.contains("> ATTENTION FEED"));
    }

    #[test]
    fn feed_rows_are_drawn_with_their_category_and_state() {
        let rendered = drawn(&app(), 100, 24);
        assert!(rendered.contains("[CAL] Sprint planning (scheduled)"));
        assert!(rendered.contains("[PR] Fix memory leak (CI failed)"));
        assert!(rendered.contains("in 15m"), "a meeting says how soon it is");
    }

    #[test]
    fn the_feed_title_counts_what_is_waiting_by_category() {
        let rendered = drawn(&app(), 100, 24);
        assert!(rendered.contains("PR 1"), "got {rendered}");
        assert!(rendered.contains("CAL 1"));
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
    fn an_empty_feed_says_what_to_do_rather_than_showing_a_blank_box() {
        let mut app = app();
        app.apply(Update::Feed(Feed::empty(NOW)));
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
    fn the_monitor_lists_the_agents_vetinari_did_not_start_and_marks_a_blocked_one() {
        let mut app = app();
        app.apply(Update::ObservedAgents(vec![
            ObservedAgentRow::from_observed(&vetinari_core::domain::agent::ObservedAgent::new(
                vetinari_core::domain::ids::SpaceId::parse("w1:p1").expect("pane"),
                Some(vetinari_core::domain::ids::SpaceId::parse("w1").expect("workspace")),
                "claude",
                vetinari_core::domain::agent::ObservedAgentState::Blocked,
                vetinari_core::domain::workspace::TargetKind::Herdr,
                None,
            )),
        ]));

        let rendered = drawn(&app, 120, 30);

        assert!(
            rendered.contains("agents Vetinari did not start"),
            "{rendered}"
        );
        assert!(
            rendered.contains("herdr claude [w1] (blocked)"),
            "{rendered}"
        );
    }

    #[test]
    fn the_monitor_lists_sessions_and_spaces() {
        let mut app = app();
        app.apply(Update::Sessions(vec![SessionRow::from_session(
            &vetinari_core::domain::agent::AgentSession::new(
                vetinari_core::domain::ids::SessionId::parse("s1").expect("id"),
                vetinari_core::domain::ids::AgentId::parse("claude-code").expect("id"),
                &vetinari_core::domain::agent::TaskSpec::new(
                    NonEmptyText::parse("writing tests").expect("prompt"),
                    vetinari_core::domain::agent::TaskClass::DeepCodeChange,
                    PathBuf::from("/repo"),
                ),
                NOW,
            ),
        )]));
        app.apply(Update::Spaces(vec![SpaceRow::from_space(
            &vetinari_core::domain::workspace::Space::new(
                vetinari_core::domain::ids::SpaceId::parse("space-1").expect("id"),
                vetinari_core::domain::workspace::SpaceName::sanitized("pr-142"),
                PathBuf::from("/trees/pr-142"),
                vetinari_core::domain::workspace::TargetKind::Herdr,
            ),
        )]));

        let rendered = drawn(&app, 120, 30);

        assert!(rendered.contains("claude-code: writing tests"));
        assert!(rendered.contains("-- spaces --"));
        assert!(rendered.contains("herdr"));
    }

    #[test]
    fn an_idle_monitor_says_how_to_start_work() {
        let rendered = drawn(&app(), 100, 24);
        assert!(rendered.contains("No agents running"));
    }

    #[test]
    fn the_details_pane_describes_the_selected_row() {
        let mut app = app();
        press(&mut app, "j");
        let rendered = drawn(&app, 120, 30);
        assert!(rendered.contains("Title: Fix memory leak"));
        assert!(rendered.contains("Author: developer"));
    }

    #[test]
    fn the_footer_lists_the_headline_bindings() {
        let rendered = drawn(&app(), 120, 24);
        assert!(rendered.contains("SPC p r: PR Review"));
        assert!(rendered.contains("SPC q: Quit"));
    }

    #[test]
    fn a_status_message_is_shown_in_its_own_line() {
        let mut app = app();
        app.apply(Update::Status(StatusMessage::error("jira is unavailable")));
        let rendered = drawn(&app, 100, 24);
        assert!(rendered.contains("jira is unavailable"));
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
}
