//! Dibujado de las pantallas.

use crate::app::{App, CONFIRM_WORD, Confirm, Cursor, Deleting, Mode, View};
use crate::delete::DeleteMode;
use crate::fmt;
use crate::model::Kind;
use crate::scan::Progress;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Table, TableState, Wrap};
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

const SIZE_W: u16 = 10;
const BAR_W: u16 = 14;
const PCT_W: u16 = 6;
const AGE_W: u16 = 7;
const MARK_W: u16 = 3;
const SELECTED: Style = Style::new().bg(Color::Rgb(45, 55, 80)).add_modifier(Modifier::BOLD);

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, list, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);
    match app.view {
        View::Explorer => draw_explorer(frame, app, list),
        View::TopFiles => draw_top(frame, app, list),
    }
    draw_footer(frame, app, footer);

    match &app.mode {
        Mode::Browse => {}
        Mode::Help => draw_help(frame),
        Mode::Confirm(confirm) => draw_confirm(frame, confirm),
        Mode::Deleting(deleting) => draw_deleting(frame, deleting),
        Mode::Failures(failures) => draw_failures(frame, failures),
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let root = fmt::sanitize(&app.root_path.to_string_lossy());
    let mut lines = vec![Line::from(vec![
        Span::styled(" optidisk ", Style::new().black().on_cyan().bold()),
        Span::raw(format!(
            " {root}  ·  {} en {} archivos y {} carpetas  ·  escaneado en {:.1} s",
            fmt::size(app.root.size),
            app.root.files,
            app.stats.dirs,
            app.stats.elapsed.as_secs_f64()
        )),
    ])];
    if let Some((total, free)) = app.volume {
        let used = total.saturating_sub(free);
        let fraction = if total > 0 { used as f64 / total as f64 } else { 0.0 };
        let color = match fraction {
            f if f >= 0.9 => Color::Red,
            f if f >= 0.75 => Color::Yellow,
            _ => Color::Green,
        };
        lines.push(Line::from(vec![
            Span::raw(" Disco "),
            Span::styled(fmt::bar(fraction, 24), Style::new().fg(color)),
            Span::raw(format!(
                " {} usados de {}  ·  {} libres",
                fmt::size(used),
                fmt::size(total),
                fmt::size(free)
            )),
        ]));
    }
    if app.stats.errors > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                " ! Sin acceso: {}. El total es un mínimo; ejecuta como administrador para medirlas.",
                fmt::count(app.stats.errors as usize, "carpeta", "carpetas")
            ),
            Style::new().yellow(),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn name_width(area: Rect) -> usize {
    area.width
        .saturating_sub(MARK_W + SIZE_W + BAR_W + PCT_W + AGE_W + 7) as usize
}

fn widths() -> [Constraint; 6] {
    [
        Constraint::Length(MARK_W),
        Constraint::Length(SIZE_W),
        Constraint::Length(BAR_W),
        Constraint::Length(PCT_W),
        Constraint::Length(AGE_W),
        Constraint::Fill(1),
    ]
}

/// Fila común a las dos vistas.
fn row(mark: bool, size: u64, parent: u64, age: String, name: Span<'static>) -> Row<'static> {
    let fraction = if parent > 0 { size as f64 / parent as f64 } else { 0.0 };
    let mark = if mark { "[x]" } else { "[ ]" };
    let style = if mark == "[x]" { Style::new().yellow() } else { Style::new().dark_gray() };
    Row::new(vec![
        Cell::from(mark).style(style),
        Cell::from(Line::from(fmt::size(size)).right_aligned()),
        Cell::from(fmt::bar(fraction, BAR_W as usize)).style(Style::new().cyan()),
        Cell::from(Line::from(format!("{:.1}%", fraction * 100.0)).right_aligned()),
        Cell::from(Line::from(age).right_aligned()).style(Style::new().dark_gray()),
        Cell::from(name),
    ])
}

fn table(rows: Vec<Row<'static>>, block: Block<'static>) -> Table<'static> {
    let header = Row::new(vec!["", "Tamaño", "", "%", "Edad", "Nombre"])
        .style(Style::new().dark_gray());
    Table::new(rows, widths())
        .header(header)
        .block(block)
        .row_highlight_style(SELECTED)
}

/// Sólo se construyen las filas visibles: una carpeta puede tener cientos de miles.
fn visible_range(cursor: &mut Cursor, len: usize, height: usize) -> std::ops::Range<usize> {
    cursor.scroll_into_view(height);
    let start = cursor.offset.min(len);
    start..(start + height).min(len)
}

fn body_block(title: Line<'static>) -> Block<'static> {
    Block::new()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::new().dark_gray())
        .title(title)
}

fn draw_explorer(frame: &mut Frame, app: &mut App, area: Rect) {
    let height = area.height.saturating_sub(3) as usize;
    app.page = height.max(1);
    let path = fmt::sanitize(&app.current_path().to_string_lossy());
    let title = fmt::fit_tail(&path, area.width.saturating_sub(4) as usize);
    let block = body_block(Line::from(format!(" {title} ")).cyan().bold());

    let base = app.current_path();
    let len = app.current_dir().children.len();
    let mut cursor = app.explorer;
    let range = visible_range(&mut cursor, len, height);
    app.explorer = cursor;

    if len == 0 {
        let message = if app.current_dir().denied { "  Sin acceso a esta carpeta" } else { "  Carpeta vacía" };
        frame.render_widget(Paragraph::new(message).block(block).dark_gray(), area);
        return;
    }

    let now = fmt::now_secs();
    let dir = app.current_dir();
    let parent = dir.size;
    let rows: Vec<Row> = dir.children[range]
        .iter()
        .map(|child| {
            let display = fmt::sanitize(&child.name.to_string_lossy());
            let name = match child.kind {
                Kind::Dir if child.incomplete => Span::styled(format!("{display}\\  !"), Style::new().yellow().bold()),
                Kind::Dir => Span::styled(format!("{display}\\"), Style::new().light_blue().bold()),
                Kind::Link => Span::styled(format!("{display} (enlace)"), Style::new().dark_gray()),
                Kind::File => Span::raw(display),
            };
            let marked = app.is_marked(&base.join(&*child.name));
            row(marked, child.size, parent, fmt::age(child.modified, now), name)
        })
        .collect();

    let mut state = TableState::default().with_selected(Some(cursor.selected - cursor.offset));
    frame.render_stateful_widget(table(rows, block), area, &mut state);
}

fn draw_top(frame: &mut Frame, app: &mut App, area: Rect) {
    app.refresh_top();
    let height = area.height.saturating_sub(3) as usize;
    app.page = height.max(1);
    let block = body_block(
        Line::from(format!(" Los {} archivos más grandes ", app.top.len())).cyan().bold(),
    );

    if app.top.is_empty() {
        frame.render_widget(Paragraph::new("  No hay archivos").block(block).dark_gray(), area);
        return;
    }

    let mut cursor = app.top_cursor;
    let range = visible_range(&mut cursor, app.top.len(), height);
    app.top_cursor = cursor;

    let now = fmt::now_secs();
    let width = name_width(area);
    let parent = app.root.size;
    let rows: Vec<Row> = app.top[range]
        .iter()
        .map(|entry| {
            let shown = entry.path.strip_prefix(&app.root_path).unwrap_or(&entry.path);
            let name = fmt::fit_tail(&fmt::sanitize(&shown.to_string_lossy()), width);
            let age = fmt::age(entry.modified, now);
            row(app.is_marked(&entry.path), entry.size, parent, age, Span::raw(name))
        })
        .collect();

    let mut state = TableState::default().with_selected(Some(cursor.selected - cursor.offset));
    frame.render_stateful_widget(table(rows, block), area, &mut state);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let info = if let Some(status) = &app.status {
        Line::from(format!(" {}", fmt::sanitize(status))).green()
    } else if !app.marks.is_empty() {
        let (count, total) = app.marked_total();
        let marked = fmt::count(count, "marcado", "marcados");
        Line::from(format!(" {marked} · {}  (Esc limpia)", fmt::size(total))).yellow()
    } else {
        Line::from("")
    };
    let hints = match app.view {
        View::Explorer => " ↑↓ mover  Enter abrir  ← subir  Espacio marcar  d papelera  D borrar  Tab top  r reescanear  ? ayuda  q salir",
        View::TopFiles => " ↑↓ mover  Enter ir a su carpeta  Espacio marcar  d papelera  D borrar  Tab explorador  ? ayuda  q salir",
    };
    let lines = vec![info, Line::from(hints).dark_gray()];
    frame.render_widget(Paragraph::new(lines), area);
}

fn popup(frame: &mut Frame, title: String, color: Color, width: u16, height: u16) -> Rect {
    let area = frame.area();
    let width = width.min(area.width);
    let height = height.min(area.height);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(color));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    inner
}

fn path_line(path: &Path, width: usize) -> String {
    fmt::fit_tail(&fmt::sanitize(&path.to_string_lossy()), width)
}

fn draw_confirm(frame: &mut Frame, confirm: &Confirm) {
    const SHOWN: usize = 8;
    let (title, color) = match confirm.mode {
        DeleteMode::Trash => (" Mover a la papelera ", Color::Yellow),
        DeleteMode::Permanent => (" BORRAR DEFINITIVAMENTE ", Color::Red),
    };
    let width = frame.area().width.saturating_sub(6).min(90);
    let inner_width = width.saturating_sub(2) as usize;

    let mut lines = vec![
        Line::from(format!(
            "{} · {}",
            fmt::count(confirm.targets.len(), "elemento", "elementos"),
            fmt::size(confirm.total())
        ))
        .bold(),
        Line::from(""),
    ];
    for (path, size) in confirm.targets.iter().take(SHOWN) {
        let size = format!("{:>10}  ", fmt::size(*size));
        lines.push(Line::from(format!(
            "{size}{}",
            path_line(path, inner_width.saturating_sub(12))
        )));
    }
    if confirm.targets.len() > SHOWN {
        lines.push(Line::from(format!("… y {} más", confirm.targets.len() - SHOWN)).dark_gray());
    }
    if !confirm.blocked.is_empty() {
        lines.push(Line::from(""));
        lines.push(
            Line::from(format!(
                "Se omiten por protección: {}",
                fmt::count(confirm.blocked.len(), "ruta", "rutas")
            ))
            .yellow(),
        );
        for (path, reason) in confirm.blocked.iter().take(3) {
            lines.push(
                Line::from(format!(
                    "  {} ({reason})",
                    path_line(path, inner_width.saturating_sub(reason.len() + 5))
                ))
                .dark_gray(),
            );
        }
    }
    for warning in &confirm.warnings {
        lines.push(Line::from(""));
        lines.push(Line::from(format!("Ojo: {warning}")).yellow().bold());
    }
    lines.push(Line::from(""));
    match confirm.mode {
        DeleteMode::Trash => {
            lines.push(Line::from("Recuperable, pero el espacio no se libera hasta vaciar la papelera."));
            lines.push(Line::from("y confirmar  ·  n / Esc cancelar").bold());
        }
        DeleteMode::Permanent => {
            lines.push(Line::from("Esto NO se puede deshacer.").red().bold());
            lines.push(Line::from(format!(
                "Escribe «{CONFIRM_WORD}» y pulsa Enter (Esc cancela): {}_",
                fmt::sanitize(&confirm.typed)
            )));
        }
    }

    // Las líneas largas se parten, así que se reserva sitio de más para ellas.
    let wrapped: u16 = lines
        .iter()
        .map(|l| (l.width().max(1) as u16).div_ceil(inner_width.max(1) as u16))
        .sum();
    let inner = popup(frame, title.into(), color, width, wrapped + 2);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_deleting(frame: &mut Frame, deleting: &Deleting) {
    let title = match deleting.mode {
        DeleteMode::Trash => " Moviendo a la papelera… ",
        DeleteMode::Permanent => " Borrando… ",
    };
    let width = frame.area().width.saturating_sub(6).min(80);
    let inner = popup(frame, title.into(), Color::Yellow, width, 7);
    let [bar, stats, path, _, hint] = Layout::vertical([Constraint::Length(1); 5]).areas(inner);
    let elapsed = deleting.started.elapsed();
    let clock = fmt::clock(elapsed.as_secs());
    let item = format!(
        "elemento {} de {}",
        (deleting.done + 1).min(deleting.total),
        deleting.total
    );

    let (stats_text, hint_text) = match deleting.mode {
        // La papelera mueve cada elemento de una vez: un porcentaje sería inventado.
        DeleteMode::Trash => {
            frame.render_widget(
                Paragraph::new(fmt::pulse(bar.width as usize, elapsed.as_millis())).yellow(),
                bar,
            );
            (
                format!("{clock}  ·  {item}  ·  {}", fmt::size(deleting.total_bytes)),
                "Cada elemento se mueve de una vez. Esc detiene tras el actual",
            )
        }
        DeleteMode::Permanent => {
            let ratio = deleting.ratio();
            let noun = if deleting.total_files == 1 { "archivo" } else { "archivos" };
            frame.render_widget(
                Gauge::default()
                    .gauge_style(Style::new().yellow().on_dark_gray())
                    .ratio(ratio)
                    .label(format!(
                        "{:.0}%  ·  {} de {} {noun}",
                        ratio * 100.0,
                        fmt::thousands(deleting.files_done()),
                        fmt::thousands(deleting.total_files)
                    )),
                bar,
            );
            (
                format!(
                    "{clock}  ·  {item}  ·  {} de {}",
                    fmt::size(deleting.bytes_done()),
                    fmt::size(deleting.total_bytes)
                ),
                "Esc detiene tras el elemento actual",
            )
        }
    };
    frame.render_widget(Paragraph::new(stats_text), stats);
    frame.render_widget(
        Paragraph::new(path_line(&deleting.current, inner.width as usize)).dark_gray(),
        path,
    );
    frame.render_widget(Paragraph::new(hint_text).dark_gray(), hint);
}

fn draw_failures(frame: &mut Frame, failures: &[(std::path::PathBuf, String)]) {
    let width = frame.area().width.saturating_sub(6).min(100);
    let inner_width = width.saturating_sub(2) as usize;
    let mut lines = vec![
        Line::from(format!(
            "No se pudo borrar: {}",
            fmt::count(failures.len(), "elemento", "elementos")
        ))
        .bold(),
        Line::from(""),
    ];
    for (path, error) in failures.iter().take(8) {
        lines.push(Line::from(path_line(path, inner_width)));
        lines.push(Line::from(format!("  {}", fmt::sanitize(error))).red());
    }
    if failures.len() > 8 {
        lines.push(Line::from(format!("… y {} más", failures.len() - 8)).dark_gray());
    }
    lines.push(Line::from(""));
    lines.push(Line::from("Si es muy grande para la papelera, usa D. Pulsa cualquier tecla.").dark_gray());
    let wrapped: u16 = lines
        .iter()
        .map(|l| (l.width().max(1) as u16).div_ceil(inner_width.max(1) as u16))
        .sum();
    let inner = popup(frame, " Errores ".into(), Color::Red, width, wrapped + 2);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_help(frame: &mut Frame) {
    const KEYS: [(&str, &str); 12] = [
        ("↑ ↓  j k", "mover la selección"),
        ("RePág AvPág", "saltar una página"),
        ("Inicio Fin  g G", "ir al principio o al final"),
        ("Enter → l", "abrir la carpeta (en el top: ir a su carpeta)"),
        ("← Retroceso h", "subir a la carpeta superior"),
        ("Tab", "alternar explorador y archivos más grandes"),
        ("Espacio", "marcar o desmarcar"),
        ("Esc", "limpiar las marcas"),
        ("d", "mover a la papelera lo marcado o seleccionado"),
        ("D", "borrar definitivamente lo marcado o seleccionado"),
        ("r", "volver a escanear"),
        ("q", "salir"),
    ];
    let mut lines: Vec<Line> = KEYS
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!("{key:<17}"), Style::new().cyan()),
                Span::raw(*what),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from("«!» = carpeta sin acceso: su tamaño real puede ser mayor.").dark_gray());
    lines.push(Line::from("Edad = última modificación del contenido.").dark_gray());
    lines.push(Line::from("Los enlaces y junctions pesan 0 y no se siguen.").dark_gray());
    let inner = popup(frame, " Ayuda ".into(), Color::Cyan, 72, lines.len() as u16 + 2);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Pantalla mientras dura el escaneo. `used` es lo ocupado en el volumen, si la raíz
/// lo cubre entero: permite estimar el avance.
pub fn draw_scanning(
    frame: &mut Frame,
    root: &Path,
    progress: &Progress,
    elapsed: Duration,
    used: Option<u64>,
) {
    let area = frame.area();
    let [_, title, stats, gauge, current, _, hint, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);

    let root_text = fmt::sanitize(&root.to_string_lossy());
    frame.render_widget(
        Paragraph::new(format!("Escaneando {root_text}")).centered().bold(),
        title,
    );

    let bytes = progress.bytes.load(Relaxed);
    let files = progress.files.load(Relaxed);
    let secs = elapsed.as_secs_f64();
    let errors = progress.errors.load(Relaxed);
    frame.render_widget(
        Paragraph::new(format!(
            "{}  ·  {} archivos  ·  {} carpetas  ·  {} sin acceso  ·  {:.0} archivos/s  ·  {:.1} s",
            fmt::size(bytes),
            files,
            progress.dirs.load(Relaxed),
            errors,
            files as f64 / secs.max(0.001),
            secs
        ))
        .centered(),
        stats,
    );

    if let Some(used) = used.filter(|u| *u > 0) {
        // Es una estimación: los enlaces duros pueden contarse varias veces.
        let ratio = (bytes as f64 / used as f64).clamp(0.0, 0.99);
        let bar_area = Rect {
            x: gauge.x + gauge.width / 6,
            y: gauge.y,
            width: gauge.width * 2 / 3,
            height: 1,
        };
        frame.render_widget(
            Gauge::default()
                .gauge_style(Style::new().cyan())
                .ratio(ratio)
                .label(format!("~{:.0}%", ratio * 100.0)),
            bar_area,
        );
    }

    let path = progress
        .current
        .lock()
        .map(|p| fmt::sanitize(&p.to_string_lossy()))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(fmt::fit_tail(&path, area.width.saturating_sub(4) as usize))
            .centered()
            .dark_gray(),
        current,
    );
    frame.render_widget(Paragraph::new("q / Esc cancelar").centered().dark_gray(), hint);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, ScanStats};
    use crate::delete::Protection;
    use crate::scan::scan;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::fs;

    fn fixture() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("carpeta")).unwrap();
        fs::write(dir.path().join("carpeta/dentro.bin"), vec![0; 3000]).unwrap();
        fs::write(dir.path().join("archivo.iso"), vec![0; 1000]).unwrap();
        fs::write(dir.path().join("ñandú 日本語.txt"), vec![0; 100]).unwrap();
        let root = scan(dir.path(), &Progress::default());
        let stats = ScanStats {
            elapsed: Duration::from_millis(1500),
            dirs: 2,
            errors: 3,
        };
        let app = App::new(dir.path().into(), root, stats, Protection::default());
        (dir, app)
    }

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .chunks(width as usize)
            .map(|line| line.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn explorer_shows_items_sizes_and_warnings() {
        let (_dir, mut app) = fixture();
        let screen = render(&mut app, 120, 20);
        assert!(screen.contains("carpeta\\"), "{screen}");
        assert!(screen.contains("archivo.iso"), "{screen}");
        assert!(screen.contains("2.9 KB"), "{screen}");
        assert!(screen.contains("Sin acceso: 3 carpetas"), "{screen}");
        assert!(screen.contains("73.2%"), "{screen}");
        let folder = screen.find("carpeta\\").unwrap();
        let file = screen.find("archivo.iso").unwrap();
        assert!(folder < file, "lo más pesado primero:\n{screen}");
    }

    #[test]
    fn every_screen_renders_at_any_size() {
        let (_dir, mut app) = fixture();
        for (width, height) in [(1, 1), (10, 3), (20, 5), (40, 10), (80, 24), (200, 60)] {
            for code in [
                KeyCode::Char('?'),
                KeyCode::Esc,
                KeyCode::Tab,
                KeyCode::Char('D'),
                KeyCode::Esc,
                KeyCode::Tab,
                KeyCode::Char('d'),
                KeyCode::Esc,
            ] {
                press(&mut app, code);
                render(&mut app, width, height);
            }
        }
    }

    #[test]
    fn confirmation_lists_targets_and_warns_about_virtual_disks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("docker_data.vhdx"), vec![0; 5000]).unwrap();
        let root = scan(dir.path(), &Progress::default());
        let stats = ScanStats {
            elapsed: Duration::ZERO,
            dirs: 1,
            errors: 0,
        };
        let mut app = App::new(dir.path().into(), root, stats, Protection::default());
        press(&mut app, KeyCode::Char('D'));
        let screen = render(&mut app, 100, 30);
        assert!(screen.contains("BORRAR DEFINITIVAMENTE"), "{screen}");
        assert!(screen.contains("docker_data.vhdx"), "{screen}");
        assert!(screen.contains("Disco virtual"), "{screen}");
        assert!(screen.contains("borrar"), "{screen}");
    }

    #[test]
    fn scrolling_keeps_the_selection_visible_in_huge_folders() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..300 {
            fs::write(dir.path().join(format!("f{i:03}")), vec![0; 1000 - i]).unwrap();
        }
        let root = scan(dir.path(), &Progress::default());
        let stats = ScanStats {
            elapsed: Duration::ZERO,
            dirs: 1,
            errors: 0,
        };
        let mut app = App::new(dir.path().into(), root, stats, Protection::default());
        render(&mut app, 80, 20);
        press(&mut app, KeyCode::End);
        let screen = render(&mut app, 80, 20);
        assert!(screen.contains("f299"), "{screen}");
        assert!(!screen.contains("f000"), "{screen}");
        press(&mut app, KeyCode::PageUp);
        assert!(app.explorer.selected < 299);
        render(&mut app, 80, 20);
    }

    #[test]
    fn deletion_screens_show_progress() {
        let (_dir, mut app) = fixture();
        press(&mut app, KeyCode::Char('D'));
        for c in CONFIRM_WORD.chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        let screen = render(&mut app, 100, 30);
        assert!(screen.contains("Borrando"), "{screen}");
        assert!(screen.contains("de 1 archivo"), "{screen}");
        assert!(screen.contains("elemento 1 de 1"), "{screen}");
    }

    #[test]
    fn scanning_screen_renders() {
        let progress = Progress::default();
        progress.bytes.store(1 << 30, Relaxed);
        progress.files.store(1234, Relaxed);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|f| {
                draw_scanning(
                    f,
                    Path::new("C:\\"),
                    &progress,
                    Duration::from_secs(2),
                    Some(2 << 30),
                )
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("1234 archivos"));
        assert!(text.contains("~50%"));
    }
}
