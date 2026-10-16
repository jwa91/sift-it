//! `sift-it` (no arguments) or `sift-it find`: search as you type, full screen. Results on the left, a preview of the
//! selected one on the right that fits its kind (the picture, PDF page or video frame inline where the terminal shows
//! pictures, a recording's waveform with the matched stretch lit, the passage or code with the query's words marked),
//! and Enter opens it where it matched.
//!
//! Three threads keep typing smooth: this one draws and reads keys; a searcher answers the latest query (exact words at
//! once, meaning as soon as the model has loaded); a previewer fetches what the selected result's preview needs.

use crate::config;
use crate::crawl::Kind;
use crate::embedder::{Embedder, ModelFiles};
use crate::open::{self, Place, timestamp};
use crate::preview;
use crate::search::{Hit, Request, Response, Searcher, words_of};
use crate::store::{Segment, Store};
use crate::term;
use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui_image::StatefulImage;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The kinds Tab steps through.
const FILTERS: [(&str, &[Kind]); 6] = [
    ("everything", &[]),
    ("documents", &[Kind::Text, Kind::Doc, Kind::Pdf]),
    ("pictures", &[Kind::Image, Kind::Screenshot]),
    ("recordings", &[Kind::Audio]),
    ("videos", &[Kind::Video]),
    ("code", &[Kind::Code]),
];

const ACCENT: Color = Color::Blue;
const MARK: Color = Color::Yellow;

enum Model {
    Loading,
    Ready,
    Missing(String),
}

/// What a result's preview shows, fetched off the drawing thread.
#[derive(Default)]
struct Preview {
    picture: Option<image::DynamicImage>,
    levels: Option<(Vec<f32>, (usize, usize))>,
    text: Option<String>,
}

/// A preview, its picture ready to draw.
struct Shown {
    picture: Option<StatefulProtocol>,
    levels: Option<(Vec<f32>, (usize, usize))>,
    text: Option<String>,
}

struct App {
    query: String,
    filter: usize,
    hits: Vec<Hit>,
    meaning: bool,
    list: ListState,
    model: Arc<Mutex<Model>>,
    previews: HashMap<i64, Shown>,
    requested: std::collections::HashSet<i64>,
    flash: Option<(String, Instant)>,
    searching: bool,
    /// The query and filter whose results are in the list.
    answered: Option<(String, usize)>,
}

impl App {
    fn selected(&self) -> Option<&Hit> {
        self.list.selected().and_then(|i| self.hits.get(i))
    }

    fn request(&self) -> Request {
        Request { q: self.query.clone(), kinds: FILTERS[self.filter].1.to_vec(), limit: 60, prefix_last: true }
    }

    fn flash(&mut self, message: impl Into<String>) {
        self.flash = Some((message.into(), Instant::now()));
    }
}

/// Runs the finder until Esc, or until a result is opened.
pub fn run(initial: &str) -> Result<()> {
    let mut terminal = ratatui::init();
    // Asked once the screen is ours: which picture protocol the terminal speaks, and its cell size.
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let outcome = run_app(&mut terminal, picker, initial);
    ratatui::restore();
    match outcome? {
        Some(place) => {
            println!("{}", open::open(&place, false)?);
            Ok(())
        }
        None => Ok(()),
    }
}

fn run_app(terminal: &mut ratatui::DefaultTerminal, picker: Picker, initial: &str) -> Result<Option<Place>> {
    let model = Arc::new(Mutex::new(Model::Loading));
    let (search_tx, search_rx) = channel::<Request>();
    let (results_tx, results_rx) = channel::<(Request, Response)>();
    let searching = spawn_searcher(search_rx, results_tx, model.clone())?;
    let (preview_tx, preview_rx) = channel::<Hit>();
    let (previews_tx, previews_rx) = channel::<(i64, Preview)>();
    spawn_previewer(preview_rx, previews_tx)?;

    let mut app = App {
        query: initial.to_string(),
        filter: 0,
        hits: Vec::new(),
        meaning: false,
        list: ListState::default(),
        model,
        previews: HashMap::new(),
        requested: Default::default(),
        flash: None,
        searching: true,
        answered: None,
    };
    search_tx.send(app.request())?;

    let outcome = (|| -> Result<Option<Place>> {
        loop {
            // Results: only the latest query's count.
            while let Ok((request, response)) = results_rx.try_recv() {
                if request.q == app.query && request.kinds == FILTERS[app.filter].1 {
                    // A new query starts at its first result; the same one answered again (with meaning, once the
                    // model is there) keeps the selection.
                    let asked = Some((request.q.clone(), app.filter));
                    let keep = if app.answered == asked { app.selected().map(|h| h.path.clone()) } else { None };
                    app.answered = asked;
                    app.hits = response.hits;
                    app.meaning = response.meaning;
                    app.searching = false;
                    let index = keep.and_then(|path| app.hits.iter().position(|h| h.path == path)).unwrap_or(0);
                    app.list.select((!app.hits.is_empty()).then_some(index));
                }
            }
            while let Ok((segment, preview)) = previews_rx.try_recv() {
                let picture = preview.picture.map(|image| picker.new_resize_protocol(image));
                app.previews.insert(segment, Shown { picture, levels: preview.levels, text: preview.text });
            }
            if let Some(hit) = app.selected().cloned()
                && app.requested.insert(hit.segment_id)
            {
                preview_tx.send(hit)?;
            }

            terminal.draw(|frame| draw(frame, &mut app))?;

            if !event::poll(Duration::from_millis(50))? {
                continue;
            }
            let Event::Key(key) = event::read()? else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match handle(&mut app, key) {
                Action::None => {}
                Action::Search => {
                    app.searching = true;
                    search_tx.send(app.request())?;
                }
                Action::Quit => return Ok(None),
                Action::Open => return Ok(app.selected().map(Place::of)),
                Action::Reveal => {
                    if let Some(hit) = app.selected() {
                        let message = open::open(&Place::of(hit), true)?;
                        app.flash(message);
                    }
                }
                Action::CopyPath => {
                    if let Some(path) = app.selected().map(|h| h.path.clone()) {
                        copy(&path)?;
                        app.flash("Copied the path");
                    }
                }
            }
        }
    })();

    // The model goes before the process does: llama.cpp frees its Metal device as the process exits, and aborts if a
    // model still holds buffers on it. With no more requests the searcher stops, and the model is freed once it and the
    // loader are done.
    drop(search_tx);
    for thread in searching {
        let _ = thread.join();
    }
    outcome
}

enum Action {
    None,
    Search,
    Quit,
    Open,
    Reveal,
    CopyPath,
}

fn handle(app: &mut App, key: KeyEvent) -> Action {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let count = app.hits.len();
    let step = |app: &mut App, by: isize| {
        if count > 0 {
            let at = app.list.selected().unwrap_or(0) as isize;
            app.list.select(Some((at + by).clamp(0, count as isize - 1) as usize));
        }
    };
    match key.code {
        KeyCode::Esc => Action::Quit,
        KeyCode::Char('c') if control => Action::Quit,
        KeyCode::Enter => Action::Open,
        KeyCode::Char('r') if control => Action::Reveal,
        KeyCode::Char('y') if control => Action::CopyPath,
        KeyCode::Down => {
            step(app, 1);
            Action::None
        }
        KeyCode::Char('n') if control => {
            step(app, 1);
            Action::None
        }
        KeyCode::Up => {
            step(app, -1);
            Action::None
        }
        KeyCode::Char('p') if control => {
            step(app, -1);
            Action::None
        }
        KeyCode::PageDown => {
            step(app, 10);
            Action::None
        }
        KeyCode::PageUp => {
            step(app, -10);
            Action::None
        }
        KeyCode::Tab => {
            app.filter = (app.filter + 1) % FILTERS.len();
            Action::Search
        }
        KeyCode::BackTab => {
            app.filter = (app.filter + FILTERS.len() - 1) % FILTERS.len();
            Action::Search
        }
        KeyCode::Char('u') if control => {
            app.query.clear();
            Action::Search
        }
        KeyCode::Char('w') if control => {
            let trimmed = app.query.trim_end().len();
            let cut = app.query[..trimmed].rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
            app.query.truncate(cut);
            Action::Search
        }
        KeyCode::Backspace => {
            app.query.pop();
            Action::Search
        }
        KeyCode::Char(c) if !control => {
            app.query.push(c);
            Action::Search
        }
        _ => Action::None,
    }
}

fn copy(text: &str) -> Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("pbcopy").stdin(std::process::Stdio::piped()).spawn()?;
    child.stdin.take().unwrap().write_all(text.as_bytes())?;
    child.wait()?;
    Ok(())
}

/// The searcher: always answers the latest request, and loads the model meanwhile (exact words work at once).
fn spawn_searcher(
    requests: Receiver<Request>,
    results: Sender<(Request, Response)>,
    model: Arc<Mutex<Model>>,
) -> Result<Vec<std::thread::JoinHandle<()>>> {
    let mut searcher = Searcher::open(&config::index_path())?;
    let encoder: Arc<Mutex<Option<Embedder>>> = Arc::new(Mutex::new(None));
    let loader = {
        let encoder = encoder.clone();
        let model = model.clone();
        std::thread::spawn(move || {
            let loaded = ModelFiles::in_folder(&config::models_dir()).and_then(|files| Embedder::load(&files, true));
            match loaded {
                Ok(embedder) => {
                    *encoder.lock().unwrap() = Some(embedder);
                    *model.lock().unwrap() = Model::Ready;
                }
                Err(error) => *model.lock().unwrap() = Model::Missing(format!("{error:#}")),
            }
        })
    };
    let searcher = std::thread::spawn(move || {
        let mut last: Option<Request> = None;
        let mut answered_with_meaning = false;
        loop {
            let request = match requests.recv_timeout(Duration::from_millis(100)) {
                Ok(mut request) => {
                    // Only the newest matters.
                    while let Ok(newer) = requests.try_recv() {
                        request = newer;
                    }
                    request
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // The model just arrived: answer the last query again, with meaning this time.
                    let ready = encoder.lock().unwrap().is_some();
                    match &last {
                        Some(request) if ready && !answered_with_meaning => request.clone(),
                        _ => continue,
                    }
                }
                Err(_) => return,
            };
            let vector = if request.q.trim().is_empty() {
                None
            } else {
                encoder.lock().unwrap().as_mut().and_then(|e| e.embed_text(&request.prompt()).ok())
            };
            answered_with_meaning = vector.is_some() || request.q.trim().is_empty();
            let response = if request.q.trim().is_empty() {
                searcher.recent(&request)
            } else {
                searcher.search(&request, vector.as_deref())
            };
            if let Ok(response) = response
                && results.send((request.clone(), response)).is_err()
            {
                return;
            }
            last = Some(request);
        }
    });
    Ok(vec![loader, searcher])
}

/// The previewer: what each kind's preview needs, made or read off the drawing thread.
fn spawn_previewer(hits: Receiver<Hit>, previews: Sender<(i64, Preview)>) -> Result<()> {
    let store = Store::open_readonly(&config::index_path())?;
    std::thread::spawn(move || {
        let folder = config::previews_dir();
        for hit in hits {
            let preview = objc2::rc::autoreleasepool(|_| make_preview(&store, &hit, &folder));
            if previews.send((hit.segment_id, preview)).is_err() {
                return;
            }
        }
    });
    Ok(())
}

fn make_preview(store: &Store, hit: &Hit, folder: &Path) -> Preview {
    let path = Path::new(&hit.path);
    let picture = |kind: &str, loc: Option<f64>| {
        preview::thumbnail(path, kind, loc, 900, folder).ok().and_then(|png| image::open(png).ok())
    };
    let levels = || {
        let columns = 80;
        let levels = preview::levels(path, columns, folder).ok()?;
        let start = hit.loc.unwrap_or(0.0);
        Some((term::stretch(&levels), preview::span(path, columns, start, hit.loc_end.unwrap_or(start + 30.0))))
    };
    let segment_text = || store.segment_text(hit.segment_id).ok().flatten();
    let mut preview = Preview::default();
    match hit.kind {
        Kind::Image => preview.picture = picture("image", None),
        Kind::Screenshot => {
            preview.picture = picture("image", None);
            preview.text =
                if hit.segment == Segment::Ocr { segment_text() } else { store.file_text(&hit.path).ok().flatten() };
        }
        Kind::Pdf => {
            preview.picture = picture("pdf", Some(hit.loc.unwrap_or(1.0)));
            preview.text = segment_text();
        }
        Kind::Video => {
            preview.picture = picture("video", hit.loc);
            if matches!(hit.segment, Segment::Sound | Segment::Speech) {
                preview.levels = levels();
            }
            if hit.segment == Segment::Speech {
                preview.text = segment_text();
            }
        }
        Kind::Audio => {
            preview.levels = levels();
            if hit.segment == Segment::Speech {
                preview.text = segment_text();
            }
        }
        Kind::Text | Kind::Doc | Kind::Code => preview.text = segment_text(),
    }
    preview
}

// Drawing.

fn draw(frame: &mut Frame, app: &mut App) {
    let [top, middle, bottom] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(4), Constraint::Length(1)]).areas(frame.area());
    draw_query(frame, app, top);
    let [left, right] = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(middle);
    draw_results(frame, app, left);
    draw_preview(frame, app, right);
    draw_keys(frame, app, bottom);
}

fn draw_query(frame: &mut Frame, app: &App, area: Rect) {
    let prompt = "sift-it ❯ ";
    let state = match &*app.model.lock().unwrap() {
        Model::Loading => " · exact words while the model loads".to_string(),
        Model::Missing(error) if error.contains("model download") => {
            " · exact words only (no model: sift-it model download)".to_string()
        }
        Model::Missing(error) => format!(" · exact words only ({})", error.chars().take(60).collect::<String>()),
        Model::Ready if app.query.trim().is_empty() => " · recently changed".to_string(),
        Model::Ready => String::new(),
    };
    let right = format!("{}{state} ", FILTERS[app.filter].0);
    let line = Line::from(vec![
        Span::styled(prompt, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw(app.query.clone()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
    let right_width = right.chars().count() as u16;
    if area.width > right_width + 20 {
        let at = Rect { x: area.x + area.width - right_width, width: right_width, ..area };
        frame.render_widget(Paragraph::new(Span::styled(right, Style::new().fg(Color::DarkGray))), at);
    }
    let x = area.x + (prompt.chars().count() + app.query.chars().count()) as u16;
    frame.set_cursor_position((x.min(area.x + area.width.saturating_sub(1)), area.y));
}

/// Where in the file it matched, said shortly.
fn place(hit: &Hit) -> String {
    match (hit.segment, hit.loc) {
        (Segment::Sound | Segment::Speech, Some(start)) => timestamp(start),
        (Segment::Frame, Some(at)) => timestamp(at),
        (_, Some(page)) if hit.kind == Kind::Pdf => format!("p. {page}"),
        (Segment::Passage, Some(cell)) if hit.path.ends_with(".ipynb") => format!("cell {cell}"),
        (Segment::Passage, Some(line)) if line > 1.0 => format!("line {line}"),
        _ => String::new(),
    }
}

fn kind_label(kind: Kind) -> &'static str {
    match kind {
        Kind::Text => "text",
        Kind::Doc => "document",
        Kind::Pdf => "pdf",
        Kind::Image => "picture",
        Kind::Screenshot => "screenshot",
        Kind::Audio => "recording",
        Kind::Video => "video",
        Kind::Code => "code",
    }
}

fn draw_results(frame: &mut Frame, app: &mut App, area: Rect) {
    let title = if app.query.trim().is_empty() {
        " recent ".to_string()
    } else if app.searching && app.hits.is_empty() {
        " searching… ".to_string()
    } else {
        format!(" {} {} ", app.hits.len(), if app.hits.len() == 1 { "match" } else { "matches" })
    };
    let block = Block::default().borders(Borders::ALL).border_style(Style::new().fg(Color::DarkGray)).title(title);
    if app.hits.is_empty() && !app.searching {
        let message = if app.query.trim().is_empty() {
            "Nothing indexed yet: sift-it folders add <folder>"
        } else {
            "Nothing stands out. Describe it differently, or check the words in quotes."
        };
        let paragraph = Paragraph::new(message).style(Style::new().fg(Color::DarkGray)).wrap(Wrap { trim: true });
        frame.render_widget(paragraph.block(block), area);
        return;
    }
    let inner_width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .hits
        .iter()
        .map(|hit| {
            let right = {
                let place = place(hit);
                if place.is_empty() { kind_label(hit.kind).to_string() } else { place }
            };
            let room = inner_width.saturating_sub(right.chars().count() + 1);
            let name: String = if hit.name.chars().count() > room {
                hit.name.chars().take(room.saturating_sub(1)).chain(std::iter::once('…')).collect()
            } else {
                hit.name.clone()
            };
            let gap = inner_width.saturating_sub(name.chars().count() + right.chars().count());
            let name_style = if hit.strong { Style::new() } else { Style::new().fg(Color::DarkGray) };
            ListItem::new(Line::from(vec![
                Span::styled(name, name_style),
                Span::raw(" ".repeat(gap)),
                Span::styled(right, Style::new().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(Color::Indexed(236)).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, area, &mut app.list);
}

fn draw_preview(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(hit) = app.selected().cloned() else {
        frame
            .render_widget(Block::default().borders(Borders::ALL).border_style(Style::new().fg(Color::DarkGray)), area);
        return;
    };
    let place = place(&hit);
    let title = if place.is_empty() { format!(" {} ", hit.name) } else { format!(" {} · {place} ", hit.name) };
    let block = Block::default().borders(Borders::ALL).border_style(Style::new().fg(Color::DarkGray)).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let words = words_of(app.query.trim_start().strip_prefix("code:").unwrap_or(&app.query));

    let facts = facts(&hit);
    let Some(shown) = app.previews.get_mut(&hit.segment_id) else {
        let waiting = Paragraph::new("…").style(Style::new().fg(Color::DarkGray));
        frame.render_widget(waiting, inner);
        return;
    };

    // Picture on top, then the waveform, the text, and the facts at the bottom.
    let picture_rows = if shown.picture.is_some() {
        if shown.text.is_some() || shown.levels.is_some() {
            inner.height * 3 / 5
        } else {
            inner.height.saturating_sub(2)
        }
    } else {
        0
    };
    let [picture_area, wave_area, text_area, facts_area] = Layout::vertical([
        Constraint::Length(picture_rows),
        Constraint::Length(if shown.levels.is_some() { 2 } else { 0 }),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(inner);
    if let Some(protocol) = shown.picture.as_mut() {
        frame.render_stateful_widget(StatefulImage::default(), picture_area, protocol);
    }
    if let Some((levels, (first, last))) = &shown.levels {
        let resampled = resample(levels, wave_area.width as usize);
        let scale = |i: usize| i * resampled.len() / levels.len().max(1);
        let (first, last) = (scale(*first), scale(*last));
        let spans: Vec<Span> = resampled
            .iter()
            .enumerate()
            .map(|(i, level)| {
                let block = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'][((level * 7.0).round() as usize).min(7)];
                let style =
                    if i >= first && i <= last { Style::new().fg(ACCENT) } else { Style::new().fg(Color::DarkGray) };
                Span::styled(block.to_string(), style)
            })
            .collect();
        let verb = if hit.segment == Segment::Speech { "said" } else { "heard" };
        let heard = match (hit.loc, hit.loc_end) {
            (Some(start), Some(end)) => format!("{verb} at {}–{}", timestamp(start), timestamp(end)),
            (Some(start), None) => format!("{verb} at {}", timestamp(start)),
            _ => String::new(),
        };
        let lines = vec![Line::from(spans), Line::styled(heard, Style::new().fg(Color::DarkGray))];
        frame.render_widget(Paragraph::new(lines), wave_area);
    }
    let text = match (&shown.text, hit.kind) {
        (Some(text), Kind::Code) => code_text(text, hit.loc, &words),
        (Some(text), _) => prose_text(text, &words),
        (None, Kind::Video) if hit.segment == Segment::Frame => Text::styled(
            hit.loc.map(|t| format!("seen at {}", timestamp(t))).unwrap_or_default(),
            Style::new().fg(Color::DarkGray),
        ),
        _ => Text::default(),
    };
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), text_area);
    frame.render_widget(Paragraph::new(Span::styled(facts, Style::new().fg(Color::DarkGray))), facts_area);
}

/// "~/Documents · 3024 × 4032 · 1.2 MB"
fn facts(hit: &Hit) -> String {
    let folder = Path::new(&hit.path).parent().map(|p| p.display().to_string()).unwrap_or_default();
    let home = config::dirs_home().display().to_string();
    let folder = folder.strip_prefix(&home).map(|rest| format!("~{rest}")).unwrap_or(folder);
    let mut parts = vec![folder];
    if let (Some(w), Some(h)) =
        (hit.info.get("width").and_then(|v| v.as_u64()), hit.info.get("height").and_then(|v| v.as_u64()))
    {
        parts.push(format!("{w} × {h}"));
    }
    if let Some(pages) = hit.info.get("pages").and_then(|v| v.as_u64()) {
        parts.push(format!("{pages} pages"));
    }
    if let Some(duration) = hit.info.get("duration").and_then(|v| v.as_f64()) {
        parts.push(timestamp(duration));
    }
    let size = hit.size as f64;
    parts.push(if size > 1e6 { format!("{:.1} MB", size / 1e6) } else { format!("{:.0} KB", (size / 1e3).max(1.0)) });
    parts.join(" · ")
}

fn resample(levels: &[f32], width: usize) -> Vec<f32> {
    if width == 0 || levels.is_empty() {
        return Vec::new();
    }
    (0..width)
        .map(|i| {
            let from = i * levels.len() / width;
            let to = ((i + 1) * levels.len() / width).max(from + 1).min(levels.len());
            levels[from..to].iter().cloned().fold(0.0, f32::max)
        })
        .collect()
}

/// A line's spans with the query's words marked (a word counts when it starts with one of them).
fn marked(line: &str, words: &[String], base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut other = String::new();
    let flush_word = |run: &mut String, spans: &mut Vec<Span<'static>>| {
        if run.is_empty() {
            return;
        }
        let lower = run.to_lowercase();
        let hit = words.iter().any(|w| w.chars().count() > 1 && lower.starts_with(w.as_str()));
        let style = if hit { Style::new().fg(MARK).add_modifier(Modifier::BOLD) } else { base };
        spans.push(Span::styled(std::mem::take(run), style));
    };
    for c in line.chars() {
        if c.is_alphanumeric() {
            if !other.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut other), base));
            }
            run.push(c);
        } else {
            flush_word(&mut run, &mut spans);
            other.push(c);
        }
    }
    flush_word(&mut run, &mut spans);
    if !other.is_empty() {
        spans.push(Span::styled(other, base));
    }
    spans
}

fn prose_text(text: &str, words: &[String]) -> Text<'static> {
    Text::from(text.lines().map(|line| Line::from(marked(line, words, Style::new()))).collect::<Vec<_>>())
}

/// Code with line numbers, from the first line with the query's words.
fn code_text(text: &str, loc: Option<f64>, words: &[String]) -> Text<'static> {
    let lines: Vec<&str> = text.lines().collect();
    let telling = |line: &str| {
        let lower = line.to_lowercase();
        words.iter().any(|w| (w.chars().count() > 3 || w.chars().any(char::is_numeric)) && lower.contains(w.as_str()))
    };
    let focus = lines.iter().position(|l| telling(l)).unwrap_or(0);
    let first = loc.map(|l| l as i64 - focus as i64);
    let shown: Vec<Line> = lines
        .iter()
        .enumerate()
        .map(|(i, code)| {
            let mut spans = vec![Span::styled(
                first.map(|n| format!("{:>4} │ ", n + i as i64)).unwrap_or_default(),
                Style::new().fg(Color::DarkGray),
            )];
            spans.extend(marked(code, words, Style::new()));
            Line::from(spans)
        })
        .collect();
    // Start a little above the line that matched.
    Text::from(shown.into_iter().skip(focus.saturating_sub(2)).collect::<Vec<_>>())
}

fn draw_keys(frame: &mut Frame, app: &mut App, area: Rect) {
    if let Some((message, at)) = &app.flash {
        if at.elapsed() < Duration::from_secs(3) {
            frame.render_widget(Paragraph::new(Span::styled(format!(" {message}"), Style::new().fg(ACCENT))), area);
            return;
        }
        app.flash = None;
    }
    let open = app.selected().map(open_label).unwrap_or_else(|| "open".into());
    let keys = [("↩", open.as_str()), ("^R", "show in Finder"), ("^Y", "copy path"), ("tab", "kind"), ("esc", "quit")];
    let mut spans = Vec::new();
    for (key, what) in keys {
        spans.push(Span::styled(format!(" {key} "), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(format!("{what}  "), Style::new().fg(Color::DarkGray)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn open_label(hit: &Hit) -> String {
    match (hit.kind, hit.loc) {
        (Kind::Audio | Kind::Video, Some(at)) => format!("play from {}", timestamp(at)),
        (Kind::Pdf, Some(page)) if page > 1.0 => format!("open at page {page}"),
        (Kind::Text | Kind::Code, Some(line)) if line > 1.0 && !hit.path.ends_with(".ipynb") => {
            format!("open at line {line}")
        }
        _ => "open".into(),
    }
}
