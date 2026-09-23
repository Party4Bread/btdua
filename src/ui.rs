//! Rendering. All state lives in `App`; this module only draws it.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

use crate::app::{App, Dialog, ExactState, Mode, SortBy};
use crate::export::Meta;
use crate::stats::{error95, estimate, fmt_count, fmt_size};
use crate::tree::{NodeId, Tree};

const BAR: usize = 20;

pub fn draw(f: &mut Frame, app: &mut App, t: &Tree, meta: &Meta) {
    let [head, modes, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    header(f, head, app, t, meta);
    mode_line(f, modes, app, t, meta);
    table(f, body, app, t, meta);
    footer(f, foot, app, t, meta);
    dialog(f, app, t, meta);
}

fn size(t: &Tree, meta: &Meta, k: f64) -> u64 {
    estimate(k, t.total_samples, meta.total_bytes)
}

fn header(f: &mut Frame, area: Rect, app: &App, t: &Tree, meta: &Meta) {
    let path = t.path_of(app.dir);
    let crumbs = if path.is_empty() { String::new() } else { format!(" ▸ {}", path.replace('/', " ▸ ")) };
    let left = Line::from(vec![
        Span::styled(" btdua ", Style::new().bold().reversed()),
        Span::raw(format!("  {}{crumbs}", meta.device)),
    ]);
    let k = app.mode.value(&t.node(app.dir).c);
    let est = size(t, meta, k);
    let rel = if est > 0 { error95(k, t.total_samples, meta.total_bytes) as f64 / est as f64 * 100.0 } else { 0.0 };
    let state = if !app.live {
        Span::styled("◆ imported", Color::Cyan)
    } else if app.deleting > 0 {
        Span::styled(format!("✖ deleting {}", app.deleting), Color::Red)
    } else if app.paused {
        Span::styled("‖ paused", Color::Yellow)
    } else {
        Span::styled("● sampling", Color::Green)
    };
    let right = Line::from(vec![
        Span::raw(format!("samples {}  ±{rel:.1}%  ", fmt_count(t.total_samples))),
        state,
        Span::raw(" "),
    ]);
    f.render_widget(Paragraph::new(left), area);
    f.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
}

fn mode_line(f: &mut Frame, area: Rect, app: &App, t: &Tree, meta: &Meta) {
    let sort = match app.sort {
        SortBy::Size => "size",
        SortBy::Name => "name",
        SortBy::Count => "samples",
    };
    let arrow = if app.reverse { "↑" } else { "↓" };
    let mut spans = vec![Span::raw(format!(" Sort: {sort} {arrow}   Mode:"))];
    for m in Mode::ALL {
        spans.push(Span::raw(" "));
        spans.push(if m == app.mode {
            Span::styled(format!("[{}]", m.label()), Style::new().bold().fg(Color::Cyan))
        } else {
            Span::styled(m.label(), Color::DarkGray)
        });
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
    if !app.marks.is_empty() {
        let excl: f64 = app.marks.iter().filter(|&&id| t.is_live(id)).map(|&id| t.node(id).c.exclusive as f64).sum();
        let txt = format!("{} marked · {} excl ", app.marks.len(), fmt_size(size(t, meta, excl)));
        f.render_widget(Paragraph::new(Span::styled(txt, Style::new().fg(Color::Yellow).bold())).alignment(Alignment::Right), area);
    }
}

fn bar(frac: f64) -> String {
    const PARTS: [&str; 8] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = (frac.clamp(0.0, 1.0) * (BAR * 8) as f64).round() as usize;
    let full = eighths / 8;
    let mut s = String::from("[");
    s.push_str(&"█".repeat(full));
    if full < BAR {
        s.push_str(PARTS[eighths % 8]);
        s.push_str(&" ".repeat(BAR - full - 1));
    }
    s.push(']');
    s
}

fn table(f: &mut Frame, area: Rect, app: &mut App, t: &Tree, meta: &Meta) {
    let entries = app.entries(t);
    let cur = app.cursor(&entries);
    let max = entries.iter().map(|&id| app.mode.value(&t.node(id).c)).fold(0.0, f64::max);
    let rows: Vec<Row> = entries
        .iter()
        .map(|&id| {
            let node = t.node(id);
            let c = &node.c;
            let v = app.mode.value(c);
            let mut cells = vec![Cell::from(format!("{:>10}", fmt_size(size(t, meta, v))))];
            if app.show_optional {
                cells.push(Cell::from(format!("{:>10}", fmt_size(size(t, meta, c.exclusive as f64)))));
                cells.push(Cell::from(format!("{:>10}", fmt_size(size(t, meta, c.shared as f64)))));
                cells.push(Cell::from(c.ratio().map_or("    -".into(), |r| format!("{r:>5.2}"))));
            }
            cells.push(Cell::from(bar(if max > 0.0 { v / max } else { 0.0 })));
            cells.push(Cell::from(format!("{:>11}", fmt_count(c.represented))));
            let marked = app.marks.contains(&id);
            let mut name = format!("{}{}", if marked { "* " } else { "  " }, node.name);
            if !node.children.is_empty() {
                name.push('/');
            }
            let mut spans = vec![Span::raw(name)];
            if node.subvol {
                spans.push(Span::styled("  ⊞subvol", Color::Cyan));
            }
            cells.push(Cell::from(Line::from(spans)));
            let mut style = Style::new();
            if node.name.starts_with('<') {
                style = style.fg(Color::Magenta);
            }
            if marked {
                style = style.fg(Color::Yellow).bold();
            }
            Row::new(cells).style(style)
        })
        .collect();
    let mut widths = vec![Constraint::Length(10)];
    let mut hdr = vec!["      Size"];
    if app.show_optional {
        widths.extend([Constraint::Length(10), Constraint::Length(10), Constraint::Length(5)]);
        hdr.extend(["      Excl", "    Shared", "Ratio"]);
    }
    widths.extend([Constraint::Length(BAR as u16 + 2), Constraint::Length(11), Constraint::Min(10)]);
    hdr.extend([" Graph", "    Samples", "  Name"]);
    let table = Table::new(rows, widths)
        .header(Row::new(hdr).style(Style::new().bold().underlined()))
        .row_highlight_style(Style::new().reversed())
        .column_spacing(1);
    let mut state = TableState::default()
        .with_offset(app.offset)
        .with_selected((!entries.is_empty()).then_some(cur));
    f.render_stateful_widget(table, area, &mut state);
    app.offset = state.offset();
    app.page = area.height.saturating_sub(1) as usize;
    if entries.is_empty() && area.height > 1 {
        let r = Rect { y: area.y + 1, height: 1, ..area };
        f.render_widget(Paragraph::new("  (empty)").style(Color::DarkGray), r);
    }
}

fn footer(f: &mut Frame, area: Rect, app: &App, t: &Tree, meta: &Meta) {
    let c = &t.node(app.dir).c;
    let left = format!(
        " Total {} · Excl {} · Shared {} · Disk {}",
        fmt_size(size(t, meta, app.mode.value(c))),
        fmt_size(size(t, meta, c.exclusive as f64)),
        fmt_size(size(t, meta, c.shared as f64)),
        fmt_size(meta.disk_bytes),
    );
    let style = Style::new().reversed();
    f.render_widget(Paragraph::new(left).style(style), area);
    f.render_widget(
        Paragraph::new("?:help  space:mark  d:delete  e:export  q:quit ").style(style).alignment(Alignment::Right),
        area,
    );
}

fn popup(f: &mut Frame, title: &str, lines: Vec<Line>, width: u16, border: Color) {
    let area = f.area();
    let w = width.min(area.width);
    let inner = w.saturating_sub(2).max(1) as usize;
    let rows: usize = lines.iter().map(|l| l.width().max(1).div_ceil(inner)).sum();
    let h = (rows as u16).saturating_add(2).min(area.height);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    let block = Block::bordered().title(format!(" {title} ")).border_style(border);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), r);
}

const HELP: &[(&str, &str)] = &[
    ("↑↓ j k PgUp PgDn g G", "move"),
    ("→ l Enter", "open directory"),
    ("← h Backspace", "parent directory"),
    ("s / n / C", "sort by size / name / samples (again: reverse)"),
    ("r", "reverse sort"),
    ("m", "cycle size mode"),
    ("c", "toggle Excl/Shared/Ratio columns"),
    ("Space / u", "mark entry / clear marks"),
    ("d", "delete marked entries (or the selected one)"),
    ("i", "info pane (x there: exact size)"),
    ("e", "export JSON"),
    ("p", "pause/resume sampling"),
    ("q", "quit"),
];

const MODE_HELP: &[(&str, &str)] = &[
    ("represented", "each sample counted once, at its canonical owner"),
    ("distributed", "shared samples split evenly across owners"),
    ("exclusive", "space freed if this entry were deleted"),
    ("shared", "space this entry shares with paths outside it"),
];

fn dialog(f: &mut Frame, app: &App, t: &Tree, meta: &Meta) {
    match &app.dialog {
        Dialog::None => {}
        Dialog::Help => {
            let mut lines: Vec<Line> = HELP
                .iter()
                .map(|(k, d)| Line::from(vec![Span::styled(format!("{k:>22}  "), Style::new().bold()), Span::raw(*d)]))
                .collect();
            lines.push(Line::raw(""));
            lines.extend(MODE_HELP.iter().map(|(k, d)| {
                Line::from(vec![Span::styled(format!("{k:>22}  "), Color::Cyan), Span::raw(*d)])
            }));
            popup(f, "Help", lines, 76, Color::Cyan);
        }
        Dialog::Info { node } => popup(f, "Info", info_lines(app, t, meta, *node), 80, Color::Cyan),
        Dialog::Confirm { targets, stage } => {
            let excl: f64 = targets.iter().map(|&id| t.node(id).c.exclusive as f64).sum();
            let mut lines = vec![
                Line::raw(format!(
                    "Delete {} item(s)? About {} would be freed (exclusive estimate).",
                    targets.len(),
                    fmt_size(size(t, meta, excl))
                )),
                Line::raw(""),
            ];
            lines.extend(targets.iter().take(10).map(|&id| Line::raw(format!("  /{}", t.path_of(id)))));
            if targets.len() > 10 {
                lines.push(Line::raw(format!("  … and {} more", targets.len() - 10)));
            }
            lines.push(Line::raw(""));
            if *stage == 2 {
                let n = targets.iter().filter(|&&id| t.has_subvol(id)).count();
                lines.push(Line::styled(
                    format!("⚠ {n} target(s) are or contain btrfs subvolumes; they will be destroyed."),
                    Style::new().fg(Color::Red).bold(),
                ));
                lines.push(Line::raw("Press y again to confirm, any other key to cancel."));
            } else {
                lines.push(Line::raw("[y] delete   [any other key] cancel"));
            }
            popup(f, "Confirm delete", lines, 80, Color::Red);
        }
        Dialog::Export { input } => popup(
            f,
            "Export",
            vec![Line::raw(format!("File: {input}█")), Line::raw(""), Line::raw("Enter: save   Esc: cancel")],
            70,
            Color::Cyan,
        ),
        Dialog::Message { title, text } => {
            let lines = text.lines().map(|l| Line::raw(l.to_string())).collect();
            popup(f, title, lines, 80, Color::Yellow);
        }
    }
}

fn info_lines(app: &App, t: &Tree, meta: &Meta, id: NodeId) -> Vec<Line<'static>> {
    let n = t.node(id);
    let c = &n.c;
    let mut lines = vec![
        Line::raw(format!("Path:        /{}", t.path_of(id))),
        Line::raw(format!("Subvolume:   {}", if n.subvol { "yes (root)" } else { "no" })),
        Line::raw(format!("Samples:     {}", fmt_count(c.represented))),
        Line::raw(format!(
            "Compression: {}",
            c.ratio().map_or("-".into(), |r| format!("{r:.2} (disk / uncompressed)"))
        )),
        Line::raw(""),
    ];
    for m in Mode::ALL {
        let k = m.value(c);
        lines.push(Line::raw(format!(
            "{:>12}  {:>10}  ± {}",
            m.label(),
            fmt_size(size(t, meta, k)),
            fmt_size(error95(k, t.total_samples, meta.total_bytes))
        )));
    }
    if !n.sharers.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::raw("Shares extents with:"));
        lines.extend(n.sharers.iter().map(|s| Line::raw(format!("  /{s}"))));
    }
    lines.push(Line::raw(""));
    lines.push(match app.exact.get(&id) {
        None if app.live => Line::styled("Exact size: press x to compute", Color::DarkGray),
        None => Line::styled("Exact size: unavailable in --import mode", Color::DarkGray),
        Some(ExactState::Running) => Line::styled("Exact size: computing…", Color::Yellow),
        Some(ExactState::Failed(e)) => Line::styled(format!("Exact size failed: {e}"), Color::Red),
        Some(ExactState::Done(e)) => Line::raw(format!(
            "Exact: disk {} · uncompressed {} · referenced {} · {} files",
            fmt_size(e.disk),
            fmt_size(e.uncompressed),
            fmt_size(e.referenced),
            fmt_count(e.files)
        )),
    });
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Sample;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn renders_at_all_sizes() {
        let mut t = Tree::new();
        t.add_sample(&Sample { owners: vec!["a/b".into(), "s/c".into()], subvols: vec!["s".into()], ratio: Some(2.0) });
        t.add_sample(&Sample::bucket("<METADATA>"));
        let meta = Meta { total_bytes: 1 << 30, disk_bytes: 1 << 31, ..Default::default() };
        let a = t.find("a").unwrap();
        for (w, h) in [(10, 3), (40, 8), (140, 40)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut app = App::new(true);
            app.marks.insert(a);
            let dialogs = [
                Dialog::None,
                Dialog::Help,
                Dialog::Info { node: t.find("a/b").unwrap() },
                Dialog::Confirm { targets: vec![a], stage: 2 },
                Dialog::Export { input: "x.json".into() },
                Dialog::message("t", "line1\nline2"),
            ];
            for d in dialogs {
                app.dialog = d;
                term.draw(|f| draw(f, &mut app, &t, &meta)).unwrap();
            }
            app.dialog = Dialog::None;
            app.dir = t.find("a/b").unwrap(); // empty directory
            term.draw(|f| draw(f, &mut app, &t, &meta)).unwrap();
        }
    }
}
