//! Interactive `nibble session search`: type to search every session, with
//! a ranked result list and a preview of the selected one.
//!
//! Looks like vert: only the terminal's 16 ANSI colours, so it follows the
//! terminal theme. Results update on every keystroke and again as the
//! background indexer loads transcript bodies.

use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::search::{self, Hit, IndexEvent, Ranker, SearchDoc};

const ACCENT: Color = Color::Blue;
const HIGHLIGHT: Color = Color::Magenta;
const MUTED: Color = Color::DarkGray;
const WARN: Color = Color::Yellow;
/// Terminals narrower than this stack the preview under the list.
const SIDE_BY_SIDE_MIN_WIDTH: u16 = 100;
/// Transcript excerpts shown in the preview.
const PREVIEW_SNIPPETS: usize = 8;
/// Context bytes either side of a match in an excerpt.
const SNIPPET_RADIUS: usize = 80;

/// What the user chose when the search closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Closed without choosing.
    Quit,
    /// Chose the doc at this index.
    Selected(usize),
}

/// What a key press asks the event loop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    None,
    Quit,
    Select(usize),
    /// Open the transcript of the doc at this index in the pager.
    Read(usize),
}

/// Search state, independent of the terminal so it can be tested.
struct App {
    docs: Vec<SearchDoc>,
    ranker: Ranker,
    query: String,
    hits: Vec<Hit>,
    list: ListState,
    preview_scroll: u16,
    indexed: usize,
    indexing: bool,
    /// Last indexer problem, shown in the status line.
    warning: Option<String>,
    /// The `--repo` filter, shown in the input title.
    scope: Option<String>,
    /// Repo path that Ctrl-F narrowed the results to: that repo and its
    /// subdirectories.
    repo_filter: Option<String>,
    /// Why the last key did nothing, shown in the title until the next key.
    notice: Option<String>,
}

impl App {
    fn new(docs: Vec<SearchDoc>, query: String) -> Self {
        let mut app = Self {
            docs,
            ranker: Ranker::default(),
            query,
            hits: Vec::new(),
            list: ListState::default(),
            preview_scroll: 0,
            indexed: 0,
            indexing: true,
            warning: None,
            scope: None,
            repo_filter: None,
            notice: None,
        };
        app.rerank();
        app
    }

    fn rerank(&mut self) {
        let keep = self.selected_doc();
        self.hits = self.ranker.rank(&self.docs, &self.query);
        if let Some(repo) = &self.repo_filter {
            let docs = &self.docs;
            self.hits.retain(|h| {
                docs[h.index]
                    .repo
                    .as_deref()
                    .is_some_and(|r| super::workspace_matches_repo(r, repo))
            });
        }
        // Keep the same session selected while bodies stream in; reset to
        // the top when the query changed it away.
        let pos = keep
            .and_then(|d| self.hits.iter().position(|h| h.index == d))
            .unwrap_or(0);
        self.list.select(if self.hits.is_empty() {
            None
        } else {
            Some(pos)
        });
    }

    fn selected_doc(&self) -> Option<usize> {
        self.list
            .selected()
            .and_then(|i| self.hits.get(i))
            .map(|h| h.index)
    }

    fn set_query(&mut self, query: String) {
        self.query = query;
        self.list.select(None);
        self.preview_scroll = 0;
        self.rerank();
    }

    fn move_selection(&mut self, delta: isize) {
        if self.hits.is_empty() {
            return;
        }
        let last = self.hits.len() - 1;
        let current = self.list.selected().unwrap_or(0);
        let next = current.saturating_add_signed(delta).min(last);
        self.list.select(Some(next));
        self.preview_scroll = 0;
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        self.notice = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => Action::Quit,
            KeyCode::Char('c') if ctrl => Action::Quit,
            KeyCode::Enter => self.selected_doc().map_or(Action::None, Action::Select),
            KeyCode::Char('o') if ctrl => self.selected_doc().map_or(Action::None, Action::Read),
            KeyCode::Up => self.step(-1),
            KeyCode::Char('p' | 'k') if ctrl => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::Char('n' | 'j') if ctrl => self.step(1),
            KeyCode::PageUp => {
                self.preview_scroll = self.preview_scroll.saturating_sub(10);
                Action::None
            }
            KeyCode::PageDown => {
                self.preview_scroll = self.preview_scroll.saturating_add(10);
                Action::None
            }
            KeyCode::Char('f') if ctrl => self.toggle_repo_filter(),
            KeyCode::Char('u') if ctrl => self.edit(|_| String::new()),
            KeyCode::Char('w') if ctrl => self.edit(|q| {
                let kept = q.trim_end();
                let cut = kept
                    .char_indices()
                    .rfind(|(_, c)| c.is_whitespace())
                    .map_or(0, |(i, c)| i + c.len_utf8());
                kept[..cut].to_string()
            }),
            KeyCode::Backspace => self.edit(|q| {
                let mut q = q.to_string();
                q.pop();
                q
            }),
            KeyCode::Char(c) if !ctrl => self.edit(|q| format!("{q}{c}")),
            _ => Action::None,
        }
    }

    /// Pasted text joins the query on one line, so a pasted newline does not
    /// act as Enter.
    fn paste(&mut self, text: &str) -> Action {
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        self.edit(|q| format!("{q}{flat}"))
    }

    /// Ctrl-F: narrow the results to the highlighted session's repo, or
    /// clear that narrowing again (back to the `--repo` set, if any). When
    /// the session has no host repo to narrow on, the title says why.
    fn toggle_repo_filter(&mut self) -> Action {
        if self.repo_filter.take().is_none() {
            let Some(doc) = self.selected_doc().map(|i| &self.docs[i]) else {
                return Action::None;
            };
            match doc.repo.as_deref().filter(|r| is_host_repo(r)) {
                Some(repo) => self.repo_filter = Some(repo.to_string()),
                None => {
                    self.notice = Some(match doc.repo.as_deref() {
                        Some(r) if !r.trim().is_empty() => {
                            format!("ctrl-f: {r} is not a repo path on this machine")
                        }
                        _ => "ctrl-f: no repo known for this session".to_string(),
                    });
                    return Action::None;
                }
            }
        }
        self.preview_scroll = 0;
        self.rerank();
        Action::None
    }

    fn step(&mut self, delta: isize) -> Action {
        self.move_selection(delta);
        Action::None
    }

    fn edit(&mut self, f: impl FnOnce(&str) -> String) -> Action {
        let query = f(&self.query);
        if query != self.query {
            self.set_query(query);
        }
        Action::None
    }

    /// Apply an indexer event; true when results may have changed.
    fn apply(&mut self, event: IndexEvent) -> bool {
        match event {
            IndexEvent::Body { index, body } => {
                if let Some(doc) = self.docs.get_mut(index) {
                    doc.body = Some(body);
                }
                self.indexed += 1;
                true
            }
            IndexEvent::Failed { index, error } => {
                let id = self
                    .docs
                    .get(index)
                    .map_or("?", |d| d.session.session_id.as_str());
                self.warning = Some(format!("could not index {}: {error}", short_id(id)));
                self.indexed += 1;
                false
            }
            IndexEvent::Done { cache_error } => {
                self.indexing = false;
                if let Some(e) = cache_error {
                    self.warning = Some(format!("search cache not saved: {e}"));
                }
                true
            }
        }
    }
}

/// Whether Ctrl-F can narrow on `repo`. A session's repo is a host path,
/// except for a sandbox session nibble cannot link to its task: that one
/// only knows its container cwd, `/<repo name>` (see
/// `sandbox::container_working_dir`), whose top-level folder does not exist
/// on the host. Narrowing on it would hide the repo's host sessions. A host
/// path whose repo was since deleted (a removed worktree, a moved
/// checkout) still counts. The root matches nothing, so it never does.
fn is_host_repo(repo: &str) -> bool {
    let path = std::path::Path::new(repo.trim_end_matches('/'));
    let mut parts = path.components();
    matches!(parts.next(), Some(std::path::Component::RootDir))
        && parts
            .next()
            .is_some_and(|top| std::path::Path::new("/").join(top).is_dir())
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Run the search UI until the user quits or picks a session. `scope`
/// names the `--repo` filter, if any, for the title. `bodies`
/// streams transcript bodies in from the background indexer; `read` opens
/// a transcript in the pager while the UI is suspended.
pub fn run(
    docs: Vec<SearchDoc>,
    query: String,
    scope: Option<String>,
    bodies: &mpsc::Receiver<IndexEvent>,
    read: impl Fn(&SearchDoc) -> Result<()>,
) -> Result<(Outcome, Vec<SearchDoc>)> {
    let mut app = App::new(docs, query);
    app.scope = scope;
    let mut terminal = init_terminal();
    let result = event_loop(&mut terminal, &mut app, bodies, &read);
    restore_terminal();
    result.map(|outcome| (outcome, app.docs))
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    bodies: &mpsc::Receiver<IndexEvent>,
    read: &impl Fn(&SearchDoc) -> Result<()>,
) -> Result<Outcome> {
    loop {
        // Drain whatever the indexer produced since the last frame and
        // rerank once, so streaming bodies cost one rank per frame.
        let mut changed = false;
        loop {
            match bodies.try_recv() {
                Ok(ev) => changed |= app.apply(ev),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if app.indexing {
                        app.indexing = false;
                        app.warning = Some("indexer stopped unexpectedly".to_string());
                    }
                    break;
                }
            }
        }
        if changed {
            app.rerank();
        }
        terminal
            .draw(|f| render(f, app))
            .context("could not draw the search UI")?;
        if !event::poll(Duration::from_millis(100)).context("could not read terminal input")? {
            continue;
        }
        let action = match event::read().context("could not read terminal input")? {
            Event::Key(key) => app.handle_key(key),
            Event::Paste(text) => app.paste(&text),
            _ => continue,
        };
        match action {
            Action::None => {}
            Action::Quit => return Ok(Outcome::Quit),
            Action::Select(i) => return Ok(Outcome::Selected(i)),
            Action::Read(i) => {
                restore_terminal();
                let shown = read(&app.docs[i]);
                *terminal = init_terminal();
                if let Err(e) = shown {
                    app.warning = Some(format!("{e:#}"));
                }
            }
        }
    }
}

/// Enter the TUI, with bracketed paste so pasted text arrives as one event.
fn init_terminal() -> ratatui::DefaultTerminal {
    let terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
    terminal
}

fn restore_terminal() {
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    ratatui::restore();
}

// ── Rendering ────────────────────────────────────────────────────────────────

fn render(f: &mut Frame, app: &mut App) {
    let [input, body, help] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .areas(f.area());
    render_input(f, app, input);
    let direction = if body.width >= SIDE_BY_SIDE_MIN_WIDTH {
        Direction::Horizontal
    } else {
        Direction::Vertical
    };
    let [list, preview] = Layout::default()
        .direction(direction)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .areas(body);
    render_list(f, app, list);
    render_preview(f, app, preview);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑↓ move · enter details · ctrl-o transcript · ctrl-f repo/all · pgup/pgdn scroll · ctrl-u clear · esc quit",
            Style::default().fg(MUTED),
        ))),
        help,
    );
}

fn render_input(f: &mut Frame, app: &App, area: Rect) {
    let mut status = format!("{} / {}", app.hits.len(), app.docs.len());
    if app.indexing {
        status.push_str(&format!(
            " · indexing transcripts {}/{}",
            app.indexed,
            app.docs.len()
        ));
    }
    let repo = app
        .repo_filter
        .as_deref()
        .map(|r| super::format_workspace(Some(r)))
        .or_else(|| app.scope.clone());
    let heading = match repo {
        Some(repo) => format!(" search sessions in {repo} "),
        None => " search sessions ".to_string(),
    };
    let mut title = vec![Span::styled(
        heading,
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )];
    if let Some(n) = &app.notice {
        title.push(Span::styled(format!(" {n} "), Style::default().fg(MUTED)));
    }
    if let Some(w) = &app.warning {
        title.push(Span::styled(format!(" {w} "), Style::default().fg(WARN)));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(Line::from(title))
        .title_bottom(
            Line::from(Span::styled(
                format!(" {status} "),
                Style::default().fg(MUTED),
            ))
            .right_aligned(),
        );
    let line = Line::from(vec![
        Span::styled("› ", Style::default().fg(ACCENT)),
        Span::raw(app.query.as_str()),
        Span::styled("▏", Style::default().fg(ACCENT)),
    ]);
    f.render_widget(Paragraph::new(line).block(block), area);
}

/// Short date: time for today, month and day this year, else full date.
fn when(doc: &SearchDoc) -> String {
    let Some(t) = doc.session.effective_time() else {
        return "—".to_string();
    };
    let dt: chrono::DateTime<chrono::Local> = t.into();
    let now = chrono::Local::now();
    if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else if dt.format("%Y").to_string() == now.format("%Y").to_string() {
        dt.format("%b %d").to_string()
    } else {
        dt.format("%Y-%m").to_string()
    }
}

fn badge(label: &'static str, on: bool) -> Span<'static> {
    if on {
        Span::styled(
            label,
            Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(label, Style::default().fg(MUTED))
    }
}

/// `text` as spans with term occurrences highlighted.
fn highlighted(text: &str, terms: &[String], base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut at = 0;
    for (start, end) in search::match_ranges(text, terms) {
        if start > at {
            spans.push(Span::styled(text[at..start].to_string(), base));
        }
        spans.push(Span::styled(
            text[start..end].to_string(),
            base.fg(HIGHLIGHT).add_modifier(Modifier::BOLD),
        ));
        at = end;
    }
    if at < text.len() {
        spans.push(Span::styled(text[at..].to_string(), base));
    }
    spans
}

fn render_list(f: &mut Frame, app: &mut App, area: Rect) {
    let terms = search::query_terms(&app.query);
    let searching = !terms.is_empty();
    let items: Vec<ListItem> = app
        .hits
        .iter()
        .map(|hit| {
            let doc = &app.docs[hit.index];
            let mut spans = Vec::new();
            if searching {
                spans.push(badge("S", hit.in_summary));
                spans.push(badge("T", hit.in_title));
                spans.push(badge("B", hit.body_count > 0));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                format!("{:>6} ", when(doc)),
                Style::default().fg(MUTED),
            ));
            spans.push(Span::styled(
                format!(
                    "{} ",
                    crate::memory::format::agent_short_name(&doc.session.agent)
                ),
                Style::default().fg(ACCENT),
            ));
            // Repo before the title: long titles are cut at the pane edge.
            spans.push(Span::styled(
                format!(
                    "{:<13}",
                    super::format_workspace(doc.repo.as_deref())
                        .chars()
                        .take(12)
                        .collect::<String>()
                ),
                Style::default().fg(MUTED),
            ));
            spans.extend(highlighted(&doc.title, &terms, Style::default()));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED))
                .title(" sessions "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▌");
    f.render_stateful_widget(list, area, &mut app.list);
}

fn section(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        title.to_string(),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ))
}

/// Preview lines for one doc: header, summary, then transcript excerpts.
fn preview_lines(
    doc: &SearchDoc,
    hit: &Hit,
    terms: &[String],
    indexing: bool,
) -> Vec<Line<'static>> {
    let s = &doc.session;
    let mut lines = vec![
        Line::from(highlighted(
            &doc.title,
            terms,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!(
                "{} · {} · {} {} · {} · {}",
                s.agent,
                doc.repo.as_deref().unwrap_or("—"),
                super::format_time(s.effective_time()),
                when(doc),
                short_id(&s.session_id),
                super::format_size(s.size_bytes),
            ),
            Style::default().fg(MUTED),
        )),
        Line::raw(""),
        section("Summary"),
    ];
    if doc.summary.is_empty() {
        lines.push(Line::from(Span::styled(
            "No summary yet (see `nibble memory summarize`).",
            Style::default().fg(MUTED),
        )));
    } else {
        lines.extend(
            doc.summary
                .lines()
                .map(|l| Line::from(highlighted(l, terms, Style::default()))),
        );
    }
    if terms.is_empty() {
        return lines;
    }
    lines.push(Line::raw(""));
    match &doc.body {
        Some(body) if hit.body_count > 0 => {
            lines.push(section(&format!(
                "In the transcript ({}{} matches)",
                hit.body_count,
                if hit.body_capped { "+" } else { "" }
            )));
            for snippet in search::snippets(body, terms, PREVIEW_SNIPPETS, SNIPPET_RADIUS) {
                let mut spans = vec![Span::styled("• ", Style::default().fg(MUTED))];
                spans.extend(highlighted(&snippet, terms, Style::default()));
                lines.push(Line::from(spans));
            }
        }
        Some(_) => lines.push(Line::from(Span::styled(
            "No matches in the transcript.",
            Style::default().fg(MUTED),
        ))),
        None if indexing => lines.push(Line::from(Span::styled(
            "Transcript not indexed yet…",
            Style::default().fg(MUTED),
        ))),
        None => lines.push(Line::from(Span::styled(
            "Transcript could not be read.",
            Style::default().fg(MUTED),
        ))),
    }
    lines
}

fn render_preview(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
        .title(" preview ");
    let terms = search::query_terms(&app.query);
    let lines = match app.list.selected().and_then(|i| app.hits.get(i)) {
        Some(hit) => preview_lines(&app.docs[hit.index], hit, &terms, app.indexing),
        None => vec![Line::from(Span::styled(
            "No sessions match.",
            Style::default().fg(MUTED),
        ))],
    };
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((app.preview_scroll, 0)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::search::Body;
    use crate::session::SessionInfo;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;

    fn doc(id: &str, title: &str, summary: &str, body: Option<&str>) -> SearchDoc {
        let mut d = SearchDoc::new(
            SessionInfo {
                agent: "claude".to_string(),
                session_id: id.to_string(),
                workspace: None,
                path: PathBuf::from("/x.jsonl"),
                modified: Some(std::time::SystemTime::now()),
                content_time: None,
                size_bytes: 2048,
            },
            title.to_string(),
            summary.to_string(),
            Some("/home/u/nibble".to_string()),
        );
        d.body = body.map(|b| Body::new(b.to_string()));
        d
    }

    fn app() -> App {
        App::new(
            vec![
                doc(
                    "aaaa1111",
                    "cron runner",
                    "added the cron daemon",
                    Some("slack delivery"),
                ),
                doc("bbbb2222", "vert keybind", "", Some("zellij pane")),
                doc("cccc3333", "unindexed", "", None),
            ],
            String::new(),
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            assert_eq!(app.handle_key(key(KeyCode::Char(c))), Action::None);
        }
    }

    fn draw(app: &mut App, width: u16, height: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
        t.draw(|f| render(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn typing_filters_and_editing_keys_work() {
        let mut app = app();
        assert_eq!(app.hits.len(), 3);
        type_str(&mut app, "zellij");
        assert_eq!(app.hits.len(), 1);
        assert_eq!(app.selected_doc(), Some(1));
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.query, "zelli");
        type_str(&mut app, "j cron");
        assert!(app.hits.is_empty());
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
        assert_eq!(app.handle_key(ctrl('o')), Action::None);
        app.handle_key(ctrl('w'));
        assert_eq!(app.query, "zellij ");
        app.handle_key(ctrl('u'));
        assert_eq!(app.query, "");
        assert_eq!(app.hits.len(), 3);
        // Ctrl-W after multi-byte whitespace (NBSP) must not panic.
        type_str(&mut app, "foo\u{a0}bar");
        app.handle_key(ctrl('w'));
        assert_eq!(app.query, "foo\u{a0}");
        app.handle_key(ctrl('w'));
        assert_eq!(app.query, "");
        // Pasted newlines become spaces instead of submitting.
        assert_eq!(app.paste("zellij\n pane"), Action::None);
        assert_eq!(app.query, "zellij pane");
        app.handle_key(ctrl('u'));
        // Backspace on an empty query is a no-op.
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.query, "");
    }

    #[test]
    fn navigation_is_clamped_and_select_returns_the_doc() {
        let mut app = app();
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.list.selected(), Some(0));
        for k in [key(KeyCode::Down), ctrl('n'), ctrl('j'), key(KeyCode::Down)] {
            app.handle_key(k);
        }
        assert_eq!(app.list.selected(), Some(2));
        app.handle_key(ctrl('p'));
        app.handle_key(ctrl('k'));
        assert_eq!(app.list.selected(), Some(0));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::Select(1));
        assert_eq!(app.handle_key(ctrl('o')), Action::Read(1));
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.preview_scroll, 10);
        app.handle_key(key(KeyCode::PageUp));
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.preview_scroll, 0);
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::Quit);
        assert_eq!(app.handle_key(ctrl('c')), Action::Quit);
        assert_eq!(app.handle_key(key(KeyCode::F(1))), Action::None);
        let mut release = key(KeyCode::Esc);
        release.kind = KeyEventKind::Release;
        assert_eq!(app.handle_key(release), Action::None);
    }

    #[test]
    fn navigation_on_empty_results_is_a_no_op() {
        let mut app = app();
        type_str(&mut app, "nothingmatches");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.list.selected(), None);
    }

    #[test]
    fn index_events_update_docs_and_keep_the_selection() {
        let mut app = app();
        type_str(&mut app, "pane");
        assert_eq!(app.selected_doc(), Some(1));
        let changed = app.apply(IndexEvent::Body {
            index: 2,
            body: Body::new("a pane here too".to_string()),
        });
        assert!(changed);
        app.rerank();
        assert_eq!(app.hits.len(), 2);
        assert_eq!(app.selected_doc(), Some(1), "selection follows the session");
        assert_eq!(app.indexed, 1);

        assert!(!app.apply(IndexEvent::Failed {
            index: 0,
            error: "boom".to_string()
        }));
        assert!(app.warning.as_deref().unwrap().contains("aaaa1111: boom"));
        assert!(app.apply(IndexEvent::Done {
            cache_error: Some("disk full".to_string())
        }));
        assert!(!app.indexing);
        assert!(app.warning.as_deref().unwrap().contains("disk full"));
        // Out-of-range index is ignored rather than panicking.
        app.apply(IndexEvent::Body {
            index: 99,
            body: Body::default(),
        });
    }

    #[test]
    fn ctrl_f_narrows_to_the_selected_repo_and_back() {
        // Repos must be host directories for Ctrl-F to narrow on them.
        let temp = tempfile::tempdir().unwrap();
        let dir = |name: &str| {
            let p = temp.path().join(name);
            std::fs::create_dir_all(&p).unwrap();
            p.to_string_lossy().to_string()
        };
        let mut app = app();
        app.docs[0].repo = Some(dir("nibble"));
        app.docs[1].repo = Some(dir("nibble-review"));
        app.docs[2].repo = Some(dir("nibble/scripts"));
        app.rerank();
        assert_eq!(app.hits.len(), 3);

        // From the nibble session: its repo and subdirectories, not the
        // sibling repo sharing a prefix. The query still applies.
        assert_eq!(app.handle_key(ctrl('f')), Action::None);
        let shown = |app: &App| app.hits.iter().map(|h| h.index).collect::<Vec<_>>();
        assert_eq!(shown(&app), vec![0, 2]);
        assert_eq!(app.selected_doc(), Some(0));
        assert!(draw(&mut app, 140, 20).contains(" search sessions in nibble "));
        type_str(&mut app, "zellij");
        assert!(app.hits.is_empty());
        app.handle_key(ctrl('u'));
        assert_eq!(shown(&app), vec![0, 2]);
        // A body streaming in while narrowed reranks inside the narrowing.
        assert!(app.apply(IndexEvent::Body {
            index: 1,
            body: Body::new("more text".to_string()),
        }));
        app.rerank();
        assert_eq!(shown(&app), vec![0, 2]);

        // Ctrl-F again clears it; the selection stays put.
        app.handle_key(key(KeyCode::Down));
        app.handle_key(ctrl('f'));
        assert_eq!(app.hits.len(), 3);
        assert_eq!(app.selected_doc(), Some(2));
        assert!(draw(&mut app, 140, 20).contains(" search sessions "));

        // The narrowed repo wins over the --repo label in the title, and
        // clearing it shows the --repo label again.
        app.scope = Some("work".to_string());
        app.handle_key(ctrl('f'));
        assert!(draw(&mut app, 140, 20).contains(" search sessions in scripts "));
        app.handle_key(ctrl('f'));
        assert!(draw(&mut app, 140, 20).contains(" search sessions in work "));

        // A repo deleted from this machine (a removed worktree) still
        // narrows: its path is a host path.
        let gone = temp.path().join("removed-worktree");
        app.docs[2].repo = Some(gone.to_string_lossy().to_string());
        app.handle_key(ctrl('f'));
        assert_eq!(app.repo_filter, app.docs[2].repo);
        assert_eq!(shown(&app), vec![2]);
        app.handle_key(ctrl('f'));

        // No-ops that say why: a container cwd, the root, no known repo.
        for (repo, why) in [
            (
                Some("/no-such-top-dir/sub"),
                "/no-such-top-dir/sub is not a repo path",
            ),
            (Some("/"), "/ is not a repo path"),
            (Some("relative/dir"), "relative/dir is not a repo path"),
            (None, "no repo known"),
        ] {
            app.docs[2].repo = repo.map(str::to_string);
            app.handle_key(ctrl('f'));
            assert!(app.repo_filter.is_none());
            assert_eq!(app.hits.len(), 3);
            assert!(draw(&mut app, 140, 20).contains(why), "{why}");
        }
        // The notice goes away on the next key.
        app.handle_key(key(KeyCode::Up));
        assert!(app.notice.is_none());
        // No selection: nothing happens and nothing is said.
        type_str(&mut app, "nothing-matches-this");
        app.handle_key(ctrl('f'));
        assert!(app.repo_filter.is_none() && app.notice.is_none());
    }

    #[test]
    fn help_line_fits_a_120_column_terminal() {
        let mut app = app();
        assert!(draw(&mut app, 120, 20).contains("esc quit"));
    }

    #[test]
    fn renders_list_badges_and_preview() {
        let mut app = app();
        type_str(&mut app, "cron");
        let screen = draw(&mut app, 140, 20);
        assert!(screen.contains("› cron"));
        assert!(screen.contains("1 / 3 · indexing transcripts 0/3"));
        assert!(screen.contains("STB"));
        assert!(screen.contains("cron runner"));
        assert!(screen.contains("Summary"));
        assert!(screen.contains("added the cron daemon"));
        assert!(screen.contains("No matches in the transcript."));
        assert!(screen.contains("nibble"));
        assert!(screen.contains(" search sessions "));
        assert!(!screen.contains("search sessions in"));

        app.scope = Some("nibble".to_string());
        assert!(draw(&mut app, 140, 20).contains(" search sessions in nibble "));
    }

    #[test]
    fn preview_shows_transcript_excerpts_and_states() {
        let mut app = app();
        type_str(&mut app, "slack");
        let screen = draw(&mut app, 140, 20);
        assert!(screen.contains("In the transcript (1 matches)"));
        assert!(screen.contains("• slack delivery"));
        assert!(screen.contains("No summary yet") || screen.contains("added the cron daemon"));

        // Unindexed body while indexing vs after indexing finished.
        let d = doc("dddd", "slack title", "", None);
        let hit = Hit {
            index: 0,
            score: 1.0,
            in_summary: false,
            in_title: true,
            body_count: 0,
            body_capped: false,
        };
        let text = |lines: Vec<Line>| {
            lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let terms = search::query_terms("slack");
        assert!(text(preview_lines(&d, &hit, &terms, true)).contains("not indexed yet"));
        assert!(text(preview_lines(&d, &hit, &terms, false)).contains("could not be read"));
        assert!(text(preview_lines(&d, &hit, &terms, false)).contains("No summary yet"));
        // Blank query: no transcript section at all.
        assert!(!text(preview_lines(&d, &hit, &[], false)).contains("transcript"));
    }

    #[test]
    fn narrow_terminals_stack_and_empty_results_say_so() {
        let mut app = app();
        app.warning = Some("careful".to_string());
        type_str(&mut app, "nomatchatall");
        let screen = draw(&mut app, 60, 24);
        assert!(screen.contains("No sessions match."));
        assert!(screen.contains("careful"));
        // Blank query hides badges.
        app.handle_key(ctrl('u'));
        app.apply(IndexEvent::Done { cache_error: None });
        let screen = draw(&mut app, 60, 24);
        assert!(!screen.contains("STB"));
        assert!(screen.contains("3 / 3 "));
        assert!(!screen.contains("indexing"));
    }

    #[test]
    fn highlighted_splits_on_matches() {
        let spans = highlighted(
            "Fix the CRON job",
            &search::query_terms("cron"),
            Style::default(),
        );
        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec!["Fix the ", "CRON", " job"]);
        assert_eq!(spans[1].style.fg, Some(HIGHLIGHT));
    }

    #[test]
    fn short_id_and_when_handle_edge_cases() {
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id("0123456789"), "01234567");
        let mut d = doc("x", "t", "", None);
        d.session.modified = None;
        assert_eq!(when(&d), "—");
        // Mid-1970 is 1970-07 in every timezone.
        d.session.modified =
            Some(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(86_400 * 190));
        assert_eq!(when(&d), "1970-07");
    }
}
