//! Estado de la interfaz y reacción a las teclas, sin dependencia de la terminal real.

use crate::delete::{self, DeleteEvent, DeleteMode, Protection};
use crate::fmt;
use crate::model::{Kind, Node, TopEntry};
use crate::scan::volume_space;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// Palabra que hay que escribir para un borrado definitivo.
pub const CONFIRM_WORD: &str = "borrar";
pub const TOP_LIMIT: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Explorer,
    TopFiles,
}

/// Qué se va a borrar, ya filtrado, a la espera de confirmación.
pub struct Confirm {
    pub mode: DeleteMode,
    pub targets: Vec<(PathBuf, u64)>,
    pub blocked: Vec<(PathBuf, &'static str)>,
    pub warnings: Vec<&'static str>,
    pub typed: String,
}

impl Confirm {
    pub fn total(&self) -> u64 {
        self.targets.iter().map(|(_, size)| size).sum()
    }
}

pub struct Deleting {
    pub mode: DeleteMode,
    pub total: usize,
    pub done: usize,
    pub current: PathBuf,
    pub cancel: Arc<AtomicBool>,
    pub started: Instant,
    pub total_files: u64,
    pub total_bytes: u64,
    /// Lo planeado para cada elemento que falta por empezar: `(archivos, bytes)`.
    plan: VecDeque<(u64, u64)>,
    current_plan: (u64, u64),
    /// Lo borrado del elemento en curso.
    current_done: (u64, u64),
    /// Lo borrado de los elementos ya terminados.
    finished_done: (u64, u64),
    freed: u64,
    failures: Vec<(PathBuf, String)>,
    rx: Receiver<DeleteEvent>,
}

impl Deleting {
    pub fn files_done(&self) -> u64 {
        self.finished_done.0 + self.current_done.0
    }

    pub fn bytes_done(&self) -> u64 {
        self.finished_done.1 + self.current_done.1
    }

    /// Avance de 0 a 1. Sin archivos (carpetas vacías, enlaces) cuenta por elementos.
    pub fn ratio(&self) -> f64 {
        let ratio = if self.total_files > 0 {
            self.files_done() as f64 / self.total_files as f64
        } else {
            self.done as f64 / self.total.max(1) as f64
        };
        ratio.clamp(0.0, 1.0)
    }

    fn start_item(&mut self, path: PathBuf) {
        self.current = path;
        self.current_plan = self.plan.pop_front().unwrap_or_default();
        self.current_done = (0, 0);
    }

    /// Las muestras se toman con el borrado en marcha: nunca hacen retroceder la barra.
    fn update_remaining(&mut self, files: u64, bytes: u64) {
        self.current_done.0 = self.current_done.0.max(self.current_plan.0.saturating_sub(files));
        self.current_done.1 = self.current_done.1.max(self.current_plan.1.saturating_sub(bytes));
    }

    /// Fija lo realmente borrado; lo que quedó de un borrado a medias no cuenta.
    fn finish_item(&mut self, remaining: Option<&Node>) {
        let (files, bytes) = remaining.map_or((0, 0), |n| (n.files, n.size));
        self.finished_done.0 += self.current_plan.0.saturating_sub(files);
        self.finished_done.1 += self.current_plan.1.saturating_sub(bytes);
        self.current_plan = (0, 0);
        self.current_done = (0, 0);
        self.done += 1;
    }
}

pub enum Mode {
    Browse,
    Help,
    Confirm(Confirm),
    Deleting(Deleting),
    /// Elementos que no se pudieron borrar.
    Failures(Vec<(PathBuf, String)>),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cursor {
    pub selected: usize,
    pub offset: usize,
}

impl Cursor {
    fn clamp(&mut self, len: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
    }

    /// Ajusta el desplazamiento para que la selección quepa en `height` filas.
    pub fn scroll_into_view(&mut self, height: usize) {
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if height > 0 && self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
    }
}

pub struct ScanStats {
    pub elapsed: Duration,
    pub dirs: u64,
    pub errors: u64,
}

pub struct App {
    pub root_path: PathBuf,
    pub root: Node,
    pub stats: ScanStats,
    pub protection: Protection,
    pub view: View,
    pub mode: Mode,
    /// Carpeta abierta, como nombres desde la raíz: los índices cambian al reordenar.
    pub cwd: Vec<Box<OsStr>>,
    pub explorer: Cursor,
    pub top_cursor: Cursor,
    pub top: Vec<TopEntry>,
    top_stale: bool,
    pub marks: BTreeMap<PathBuf, u64>,
    pub status: Option<String>,
    pub volume: Option<(u64, u64)>,
    /// Filas visibles en el último dibujado, para saltar de página.
    pub page: usize,
    pub quit: bool,
    pub rescan: bool,
}

impl App {
    pub fn new(root_path: PathBuf, root: Node, stats: ScanStats, protection: Protection) -> Self {
        let volume = volume_space(&root_path);
        Self {
            root_path,
            root,
            stats,
            protection,
            view: View::Explorer,
            mode: Mode::Browse,
            cwd: Vec::new(),
            explorer: Cursor::default(),
            top_cursor: Cursor::default(),
            top: Vec::new(),
            top_stale: true,
            marks: BTreeMap::new(),
            status: None,
            volume,
            page: 10,
            quit: false,
            rescan: false,
        }
    }

    pub fn current_dir(&self) -> &Node {
        self.root.descend(&self.cwd).unwrap_or(&self.root)
    }

    pub fn current_path(&self) -> PathBuf {
        let mut path = self.root_path.clone();
        path.extend(self.cwd.iter().map(|name| &**name));
        path
    }

    /// Recalcula la lista global sólo si cambió el árbol.
    pub fn refresh_top(&mut self) {
        if self.top_stale {
            self.top = self.root.top_files(&self.root_path, TOP_LIMIT);
            self.top_stale = false;
            self.top_cursor.clamp(self.top.len());
        }
    }

    pub fn is_marked(&self, path: &Path) -> bool {
        self.marks.keys().any(|mark| path.starts_with(mark))
    }

    /// Lo marcado, sin contar dos veces lo que cuelga de otra marca.
    pub fn marked_total(&self) -> (usize, u64) {
        let kept = delete::without_nested(&self.marks);
        (kept.len(), kept.iter().map(|(_, size)| size).sum())
    }

    fn selection(&mut self) -> Option<(PathBuf, u64)> {
        match self.view {
            View::Explorer => {
                let child = self.current_dir().children.get(self.explorer.selected)?;
                let (name, size) = (child.name.clone(), child.size);
                Some((self.current_path().join(&*name), size))
            }
            View::TopFiles => {
                self.refresh_top();
                let entry = self.top.get(self.top_cursor.selected)?;
                Some((entry.path.clone(), entry.size))
            }
        }
    }

    fn visible_len(&self) -> usize {
        match self.view {
            View::Explorer => self.current_dir().children.len(),
            View::TopFiles => self.top.len(),
        }
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        match self.view {
            View::Explorer => &mut self.explorer,
            View::TopFiles => &mut self.top_cursor,
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let last = self.visible_len().saturating_sub(1);
        let cursor = self.cursor_mut();
        cursor.selected = cursor.selected.saturating_add_signed(delta).min(last);
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        match &mut self.mode {
            Mode::Browse => {
                if ctrl_c {
                    self.quit = true;
                } else {
                    self.handle_browse(key.code);
                }
            }
            Mode::Help | Mode::Failures(_) => self.mode = Mode::Browse,
            Mode::Confirm(_) => self.handle_confirm(key.code, ctrl_c),
            Mode::Deleting(deleting) => {
                if ctrl_c || key.code == KeyCode::Esc {
                    deleting.cancel.store(true, Relaxed);
                    self.status = Some("Deteniendo tras el elemento actual…".into());
                }
            }
        }
    }

    fn handle_browse(&mut self, code: KeyCode) {
        self.status = None;
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('r') => self.rescan = true,
            KeyCode::Tab => {
                self.view = match self.view {
                    View::Explorer => View::TopFiles,
                    View::TopFiles => View::Explorer,
                };
                self.refresh_top();
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-(self.page as isize)),
            KeyCode::PageDown => self.move_cursor(self.page as isize),
            KeyCode::Home | KeyCode::Char('g') => self.cursor_mut().selected = 0,
            KeyCode::End | KeyCode::Char('G') => self.move_cursor(isize::MAX),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.open_selection(),
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.go_up(),
            KeyCode::Char(' ') => self.toggle_mark(),
            KeyCode::Esc => self.marks.clear(),
            KeyCode::Char('d') => self.ask_delete(DeleteMode::Trash),
            KeyCode::Char('D') => self.ask_delete(DeleteMode::Permanent),
            _ => {}
        }
    }

    fn open_selection(&mut self) {
        match self.view {
            View::Explorer => {
                let Some(child) = self.current_dir().children.get(self.explorer.selected) else {
                    return;
                };
                if child.kind == Kind::Dir {
                    let name = child.name.clone();
                    self.cwd.push(name);
                    self.explorer = Cursor::default();
                }
            }
            // Desde la lista global, «abrir» lleva a la carpeta del archivo para ver su contexto.
            View::TopFiles => {
                let Some(entry) = self.top.get(self.top_cursor.selected) else {
                    return;
                };
                let names = self.relative_names(&entry.path.clone());
                if let Some((name, parents)) = names.split_last() {
                    self.cwd = parents.to_vec();
                    self.view = View::Explorer;
                    self.explorer = Cursor {
                        selected: self.position_of(name),
                        offset: 0,
                    };
                }
            }
        }
    }

    fn position_of(&self, name: &OsStr) -> usize {
        self.current_dir()
            .children
            .iter()
            .position(|c| &*c.name == name)
            .unwrap_or(0)
    }

    fn go_up(&mut self) {
        if self.view == View::Explorer
            && let Some(left) = self.cwd.pop() {
                self.explorer = Cursor {
                    selected: self.position_of(&left),
                    offset: 0,
                };
            }
    }

    fn toggle_mark(&mut self) {
        let Some((path, size)) = self.selection() else {
            return;
        };
        if self.marks.remove(&path).is_none() {
            self.marks.insert(path, size);
        }
        self.move_cursor(1);
    }

    fn ask_delete(&mut self, mode: DeleteMode) {
        let candidates = if self.marks.is_empty() {
            self.selection().into_iter().collect()
        } else {
            delete::without_nested(&self.marks)
        };
        if candidates.is_empty() {
            return;
        }
        let mut confirm = Confirm {
            mode,
            targets: Vec::new(),
            blocked: Vec::new(),
            warnings: Vec::new(),
            typed: String::new(),
        };
        for (path, size) in candidates {
            if let Some(reason) = self.protection.blocked(&path) {
                confirm.blocked.push((path, reason));
                continue;
            }
            if let Some(warning) = self.protection.warning(&path)
                && !confirm.warnings.contains(&warning) {
                    confirm.warnings.push(warning);
                }
            confirm.targets.push((path, size));
        }
        if confirm.targets.is_empty() {
            let (path, reason) = &confirm.blocked[0];
            self.status = Some(format!("No se puede borrar {}: {reason}", path.display()));
            return;
        }
        self.mode = Mode::Confirm(confirm);
    }

    fn handle_confirm(&mut self, code: KeyCode, ctrl_c: bool) {
        let Mode::Confirm(confirm) = &mut self.mode else {
            return;
        };
        if ctrl_c || code == KeyCode::Esc {
            self.mode = Mode::Browse;
            return;
        }
        let accepted = match confirm.mode {
            DeleteMode::Trash => match code {
                KeyCode::Char('y' | 'Y' | 's' | 'S') => true,
                KeyCode::Char('n' | 'N') => {
                    self.mode = Mode::Browse;
                    false
                }
                _ => false,
            },
            DeleteMode::Permanent => match code {
                KeyCode::Char(c) => {
                    confirm.typed.push(c);
                    false
                }
                KeyCode::Backspace => {
                    confirm.typed.pop();
                    false
                }
                KeyCode::Enter => confirm.typed == CONFIRM_WORD,
                _ => false,
            },
        };
        if accepted {
            self.start_delete();
        }
    }

    fn start_delete(&mut self) {
        let Mode::Confirm(confirm) = std::mem::replace(&mut self.mode, Mode::Browse) else {
            return;
        };
        let plan: VecDeque<(u64, u64)> = confirm
            .targets
            .iter()
            .map(|(path, size)| {
                let files = self.root.descend(&self.relative_names(path)).map_or(1, |n| n.files);
                (files, *size)
            })
            .collect();
        let targets: Vec<PathBuf> = confirm.targets.into_iter().map(|(path, _)| path).collect();
        let cancel = Arc::new(AtomicBool::new(false));
        self.mode = Mode::Deleting(Deleting {
            mode: confirm.mode,
            total: targets.len(),
            done: 0,
            current: PathBuf::new(),
            cancel: cancel.clone(),
            started: Instant::now(),
            total_files: plan.iter().map(|(files, _)| files).sum(),
            total_bytes: plan.iter().map(|(_, bytes)| bytes).sum(),
            plan,
            current_plan: (0, 0),
            current_done: (0, 0),
            finished_done: (0, 0),
            freed: 0,
            failures: Vec::new(),
            rx: delete::spawn(targets, confirm.mode, cancel),
        });
    }

    /// Aplica lo que haya terminado el hilo de borrado.
    pub fn tick(&mut self) {
        loop {
            let Mode::Deleting(deleting) = &mut self.mode else {
                return;
            };
            match deleting.rx.try_recv() {
                Ok(DeleteEvent::Started(path)) => deleting.start_item(path),
                Ok(DeleteEvent::Remaining { files, bytes }) => {
                    deleting.update_remaining(files, bytes);
                }
                Ok(DeleteEvent::Finished {
                    path,
                    error,
                    remaining,
                }) => {
                    deleting.finish_item(remaining.as_ref());
                    if let Some(error) = error {
                        deleting.failures.push((path.clone(), error));
                    }
                    let freed = self.apply_outcome(&path, remaining);
                    if let Mode::Deleting(deleting) = &mut self.mode {
                        deleting.freed += freed;
                    }
                }
                Ok(DeleteEvent::Done) | Err(TryRecvError::Disconnected) => {
                    self.finish_delete();
                    return;
                }
                Err(TryRecvError::Empty) => return,
            }
        }
    }

    fn finish_delete(&mut self) {
        let Mode::Deleting(deleting) = std::mem::replace(&mut self.mode, Mode::Browse) else {
            return;
        };
        self.volume = volume_space(&self.root_path);
        self.status = Some(match deleting.mode {
            DeleteMode::Trash => format!(
                "{} movidos a la papelera: el espacio no se libera hasta vaciarla",
                fmt::size(deleting.freed)
            ),
            DeleteMode::Permanent => format!("{} liberados", fmt::size(deleting.freed)),
        });
        if !deleting.failures.is_empty() {
            self.mode = Mode::Failures(deleting.failures);
        }
    }

    /// Refleja en el árbol lo que quedó en disco y devuelve los bytes que desaparecieron.
    fn apply_outcome(&mut self, path: &Path, remaining: Option<Node>) -> u64 {
        let names = self.relative_names(path);
        let before = self.root.descend(&names).map_or(0, |n| n.size);
        let after = remaining.as_ref().map_or(0, |n| n.size);
        self.root.update_at(&names, remaining);
        self.top_stale = true;
        // También tras un fallo: quien lo vea en el aviso decide si volver a marcarlo.
        self.marks.retain(|mark, _| !mark.starts_with(path));

        // La carpeta abierta pudo desaparecer si estaba marcada o dentro de algo marcado.
        while !self.cwd.is_empty() && self.root.descend(&self.cwd).is_none() {
            self.cwd.pop();
            self.explorer = Cursor::default();
        }
        let len = self.current_dir().children.len();
        self.explorer.clamp(len);
        before.saturating_sub(after)
    }

    fn relative_names(&self, path: &Path) -> Vec<Box<OsStr>> {
        path.strip_prefix(&self.root_path)
            .map(|rel| {
                rel.components()
                    .filter_map(|c| match c {
                        Component::Normal(name) => Some(name.into()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Progress, scan};
    use ratatui::crossterm::event::KeyModifiers;
    use std::fs;
    use std::time::Instant;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(key(code));
    }

    fn type_word(app: &mut App, word: &str) {
        for c in word.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    /// raiz/ { grande/ {a=1000, b=500}, medio=800, poco=10 }
    fn fixture() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        fs::create_dir(base.join("grande")).unwrap();
        fs::write(base.join("grande/a"), vec![0; 1000]).unwrap();
        fs::write(base.join("grande/b"), vec![0; 500]).unwrap();
        fs::write(base.join("medio"), vec![0; 800]).unwrap();
        fs::write(base.join("poco"), vec![0; 10]).unwrap();
        let root = scan(base, &Progress::default());
        let stats = ScanStats {
            elapsed: Duration::from_millis(1),
            dirs: 2,
            errors: 0,
        };
        let app = App::new(base.to_path_buf(), root, stats, Protection::default());
        (dir, app)
    }

    fn wait_for_delete(app: &mut App) {
        let start = Instant::now();
        while matches!(app.mode, Mode::Deleting(_)) {
            assert!(start.elapsed() < Duration::from_secs(10), "borrado colgado");
            app.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn items_are_listed_heaviest_first_and_navigation_works() {
        let (_dir, mut app) = fixture();
        assert_eq!(app.root.size, 2310);
        assert_eq!(app.selection().unwrap().1, 1500);

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.cwd.len(), 1);
        assert_eq!(app.current_dir().children[0].size, 1000);

        press(&mut app, KeyCode::Down);
        assert_eq!(app.selection().unwrap().1, 500);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.explorer.selected, 1, "no pasa del final");

        press(&mut app, KeyCode::Backspace);
        assert!(app.cwd.is_empty());
        assert_eq!(app.explorer.selected, 0, "vuelve a la carpeta de la que venía");

        press(&mut app, KeyCode::End);
        assert_eq!(app.explorer.selected, 2);
        press(&mut app, KeyCode::Home);
        assert_eq!(app.explorer.selected, 0);
    }

    #[test]
    fn files_cannot_be_entered() {
        let (_dir, mut app) = fixture();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert!(app.cwd.is_empty());
    }

    #[test]
    fn top_view_lists_files_globally_and_jumps_to_their_folder() {
        let (_dir, mut app) = fixture();
        press(&mut app, KeyCode::Tab);
        let sizes: Vec<_> = app.top.iter().map(|e| e.size).collect();
        assert_eq!(sizes, [1000, 800, 500, 10]);

        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.view, View::Explorer);
        assert_eq!(app.cwd.len(), 1);
        assert_eq!(app.selection().unwrap().1, 500);
    }

    #[test]
    fn permanent_delete_requires_the_exact_word() {
        let (dir, mut app) = fixture();
        let target = dir.path().join("grande");

        press(&mut app, KeyCode::Char('D'));
        assert!(matches!(app.mode, Mode::Confirm(_)));
        press(&mut app, KeyCode::Enter);
        type_word(&mut app, "y");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Confirm(_)), "«y» no basta");
        assert!(target.exists());

        press(&mut app, KeyCode::Backspace);
        type_word(&mut app, CONFIRM_WORD);
        press(&mut app, KeyCode::Enter);
        wait_for_delete(&mut app);

        assert!(!target.exists());
        assert_eq!(app.root.size, 810);
        assert_eq!(app.root.files, 2);
        assert_eq!(app.status.as_deref(), Some("1.5 KB liberados"));
        assert_eq!(app.selection().unwrap().1, 800);
    }

    fn deleting_with_plan(plan: &[(u64, u64)]) -> Deleting {
        let (_tx, rx) = std::sync::mpsc::channel();
        Deleting {
            mode: DeleteMode::Permanent,
            total: plan.len(),
            done: 0,
            current: PathBuf::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
            total_files: plan.iter().map(|p| p.0).sum(),
            total_bytes: plan.iter().map(|p| p.1).sum(),
            plan: plan.iter().copied().collect(),
            current_plan: (0, 0),
            current_done: (0, 0),
            finished_done: (0, 0),
            freed: 0,
            failures: Vec::new(),
            rx,
        }
    }

    #[test]
    fn progress_advances_across_items_and_never_goes_back() {
        let mut d = deleting_with_plan(&[(100, 1000), (300, 3000)]);
        assert_eq!(d.ratio(), 0.0);

        d.start_item("a".into());
        d.update_remaining(60, 600);
        assert_eq!((d.files_done(), d.bytes_done()), (40, 400));
        assert_eq!(d.ratio(), 0.1);
        d.update_remaining(80, 800); // una muestra atrasada
        assert_eq!(d.files_done(), 40);
        d.finish_item(None);
        assert_eq!((d.files_done(), d.bytes_done()), (100, 1000));

        d.start_item("b".into());
        assert_eq!(d.files_done(), 100);
        d.update_remaining(0, 0);
        assert_eq!(d.ratio(), 1.0);
    }

    #[test]
    fn a_partial_delete_counts_only_what_really_went_away() {
        let mut d = deleting_with_plan(&[(100, 1000)]);
        d.start_item("a".into());
        d.update_remaining(0, 0);
        let mut left = Node::file(OsStr::new("a"), 250, 0);
        left.files = 25;
        d.finish_item(Some(&left));
        assert_eq!((d.files_done(), d.bytes_done()), (75, 750));
        assert_eq!(d.ratio(), 0.75);
    }

    #[test]
    fn items_without_files_progress_by_item() {
        let mut d = deleting_with_plan(&[(0, 0), (0, 0)]);
        assert_eq!(d.ratio(), 0.0);
        d.start_item("enlace".into());
        d.finish_item(None);
        assert_eq!(d.ratio(), 0.5);
    }

    #[test]
    fn cancelling_a_confirmation_deletes_nothing() {
        let (dir, mut app) = fixture();
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('n'));
        assert!(matches!(app.mode, Mode::Browse));
        press(&mut app, KeyCode::Char('D'));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Browse));
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Enter); // Enter no confirma la papelera
        press(&mut app, KeyCode::Esc);
        assert!(dir.path().join("grande").exists());
    }

    #[test]
    fn marked_items_are_deleted_together_without_double_counting() {
        let (dir, mut app) = fixture();
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char(' ')); // grande/a
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Char(' ')); // grande, que ya contiene a grande/a
        press(&mut app, KeyCode::Char(' ')); // medio
        assert_eq!(app.marked_total(), (2, 2300));
        assert!(app.is_marked(&dir.path().join("grande/b")));

        press(&mut app, KeyCode::Char('D'));
        type_word(&mut app, CONFIRM_WORD);
        press(&mut app, KeyCode::Enter);
        wait_for_delete(&mut app);

        assert!(!dir.path().join("grande").exists());
        assert!(!dir.path().join("medio").exists());
        assert!(dir.path().join("poco").exists());
        assert!(app.marks.is_empty());
        assert_eq!(app.root.size, 10);
    }

    #[test]
    fn protected_paths_are_refused_or_skipped() {
        let (dir, mut app) = fixture();
        let grande = dir.path().join("grande");
        app.protection = Protection::from_vars(|name| {
            (name == "SystemRoot").then(|| grande.clone())
        });

        press(&mut app, KeyCode::Char('D'));
        assert!(matches!(app.mode, Mode::Browse), "sin nada que borrar no se pregunta");
        assert!(app.status.as_deref().unwrap().contains("parte de Windows"));

        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char('D'));
        let Mode::Confirm(confirm) = &app.mode else {
            panic!("debe pedir confirmación por lo no protegido");
        };
        assert_eq!(confirm.targets.len(), 1);
        assert_eq!(confirm.blocked.len(), 1);
        assert_eq!(confirm.total(), 800);

        type_word(&mut app, CONFIRM_WORD);
        press(&mut app, KeyCode::Enter);
        wait_for_delete(&mut app);
        assert!(dir.path().join("grande").exists());
        assert!(!dir.path().join("medio").exists());
    }

    #[test]
    fn failures_are_reported_and_the_tree_matches_the_disk() {
        let (dir, mut app) = fixture();
        // El archivo desaparece por fuera entre el escaneo y el borrado.
        fs::remove_file(dir.path().join("medio")).unwrap();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('D'));
        type_word(&mut app, CONFIRM_WORD);
        press(&mut app, KeyCode::Enter);
        wait_for_delete(&mut app);

        let Mode::Failures(failures) = &app.mode else {
            panic!("debe informar del fallo");
        };
        assert_eq!(failures.len(), 1);
        assert_eq!(app.root.size, 1510, "el árbol refleja que ya no está");
        press(&mut app, KeyCode::Char('x'));
        assert!(matches!(app.mode, Mode::Browse));
    }

    #[test]
    fn a_locked_file_keeps_the_rest_of_the_tree_accurate() {
        let (dir, mut app) = fixture();
        // Un archivo abierto sin compartir borrado no se puede eliminar: el borrado
        // de la carpeta queda a medias y hay que volver a medir lo que quedó.
        #[cfg(windows)]
        let _lock = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(dir.path().join("grande/a"))
                .unwrap()
        };
        press(&mut app, KeyCode::Char('D'));
        type_word(&mut app, CONFIRM_WORD);
        press(&mut app, KeyCode::Enter);
        wait_for_delete(&mut app);

        if cfg!(windows) {
            assert!(matches!(app.mode, Mode::Failures(_)));
            assert!(dir.path().join("grande/a").exists());
            let on_disk = scan(dir.path(), &Progress::default()).size;
            assert_eq!(app.root.size, on_disk, "el árbol coincide con el disco");
        }
    }

    #[test]
    fn deleting_the_open_folder_moves_up() {
        let (dir, mut app) = fixture();
        press(&mut app, KeyCode::Char(' ')); // marca grande/
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.cwd.len(), 1);

        press(&mut app, KeyCode::Char('D'));
        type_word(&mut app, CONFIRM_WORD);
        press(&mut app, KeyCode::Enter);
        wait_for_delete(&mut app);

        assert!(!dir.path().join("grande").exists());
        assert!(app.cwd.is_empty());
        assert_eq!(app.current_dir().children.len(), 2);
    }

    #[test]
    fn deleting_from_the_top_view_updates_the_open_folder() {
        let (dir, mut app) = fixture();
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Tab);
        // Marca los dos archivos de grande/: posiciones 0 y 2 de la lista global.
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char('D'));
        type_word(&mut app, CONFIRM_WORD);
        press(&mut app, KeyCode::Enter);
        wait_for_delete(&mut app);

        assert_eq!(app.top.len(), 4, "se recalcula de forma perezosa");
        app.refresh_top();
        assert_eq!(app.top.len(), 2);
        assert!(!dir.path().join("grande/a").exists());
        assert!(!dir.path().join("grande/b").exists());
        assert_eq!(app.current_dir().size, 0);
    }

    #[test]
    fn escape_clears_marks_and_keys_quit() {
        let (_dir, mut app) = fixture();
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.marks.len(), 1);
        press(&mut app, KeyCode::Esc);
        assert!(app.marks.is_empty());
        press(&mut app, KeyCode::Char('r'));
        assert!(app.rescan);
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn empty_directories_are_safe_to_browse() {
        let dir = tempfile::tempdir().unwrap();
        let root = scan(dir.path(), &Progress::default());
        let stats = ScanStats {
            elapsed: Duration::ZERO,
            dirs: 1,
            errors: 0,
        };
        let mut app = App::new(dir.path().into(), root, stats, Protection::default());
        for code in [
            KeyCode::Down,
            KeyCode::End,
            KeyCode::PageDown,
            KeyCode::Enter,
            KeyCode::Char(' '),
            KeyCode::Char('d'),
            KeyCode::Tab,
            KeyCode::Enter,
            KeyCode::Char('D'),
        ] {
            press(&mut app, code);
        }
        assert!(matches!(app.mode, Mode::Browse));
    }

    #[test]
    fn cursor_scrolls_only_when_needed() {
        let mut cursor = Cursor {
            selected: 12,
            offset: 0,
        };
        cursor.scroll_into_view(10);
        assert_eq!(cursor.offset, 3);
        cursor.selected = 5;
        cursor.scroll_into_view(10);
        assert_eq!(cursor.offset, 3);
        cursor.selected = 1;
        cursor.scroll_into_view(10);
        assert_eq!(cursor.offset, 1);
    }
}
