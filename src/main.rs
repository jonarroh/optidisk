use optidisk::app::{App, ScanStats};
use optidisk::delete::Protection;
use optidisk::fmt;
use optidisk::model::Node;
use optidisk::scan::{Progress, scan, volume_space};
use optidisk::ui;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::thread;
use std::time::{Duration, Instant};

const USAGE: &str = "\
optidisk: encuentra lo que más pesa en tu disco y bórralo desde un TUI

USO:
    optidisk [RUTA] [OPCIONES]

ARGUMENTOS:
    RUTA              Carpeta o unidad a analizar (por defecto, la unidad del sistema)

OPCIONES:
    -t, --threads N   Hilos del escaneo (por defecto, los núcleos lógicos)
        --print N     No abre el TUI: escanea e imprime los N archivos más grandes
    -h, --help        Muestra esta ayuda

Pulsa ? dentro del TUI para ver las teclas.";

struct Options {
    root: PathBuf,
    threads: usize,
    print: Option<usize>,
}

fn default_root() -> PathBuf {
    let drive = std::env::var_os("SystemDrive").unwrap_or_else(|| "/".into());
    let mut root = PathBuf::from(drive);
    root.push(std::path::MAIN_SEPARATOR_STR);
    root
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Options>, String> {
    let mut root = None;
    let mut threads = 0;
    let mut print = None;
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-t" | "--threads" => threads = number(&arg, args.next())?,
            "--print" => print = Some(number(&arg, args.next())?),
            flag if flag.starts_with('-') => return Err(format!("opción desconocida: {flag}")),
            path if root.is_none() => root = Some(PathBuf::from(path)),
            extra => return Err(format!("argumento de más: {extra}")),
        }
    }
    Ok(Some(Options {
        root: root.unwrap_or_else(default_root),
        threads,
        print,
    }))
}

fn number(flag: &str, value: Option<String>) -> Result<usize, String> {
    value
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("{flag} necesita un número"))
}

fn main() {
    // Todo corre en un hilo con pila grande: recorrer o soltar un árbol es recursivo.
    let code = thread::Builder::new()
        .name("main".into())
        .stack_size(optidisk::DEEP_STACK)
        .spawn(real_main)
        .expect("no se pudo crear el hilo principal")
        .join()
        .unwrap_or(1);
    // Sin liberar el árbol nodo a nodo: con millones de nodos, salir tardaría segundos.
    std::process::exit(code);
}

fn real_main() -> i32 {
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => {
            println!("{USAGE}");
            return 0;
        }
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            return 2;
        }
    };
    let root = match prepare_root(&options.root) {
        Ok(root) => root,
        Err(message) => {
            eprintln!("error: {message}");
            return 1;
        }
    };
    if let Err(error) = rayon::ThreadPoolBuilder::new()
        .num_threads(options.threads)
        .stack_size(optidisk::DEEP_STACK)
        .thread_name(|i| format!("scan-{i}"))
        .build_global()
    {
        eprintln!("error: no se pudieron crear los hilos: {error}");
        return 1;
    }

    if let Some(limit) = options.print {
        print_report(&root, limit);
        return 0;
    }

    let mut terminal = ratatui::init();
    let result = run_tui(&mut terminal, root);
    ratatui::restore();
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

fn prepare_root(path: &Path) -> Result<PathBuf, String> {
    let root = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
    match std::fs::metadata(&root) {
        Ok(meta) if meta.is_dir() => Ok(root),
        Ok(_) => Err(format!("{} no es una carpeta", root.display())),
        Err(error) => Err(format!("{}: {error}", root.display())),
    }
}

fn print_report(root: &Path, limit: usize) {
    let progress = Progress::default();
    let start = Instant::now();
    let tree = scan(root, &progress);
    let elapsed = start.elapsed();
    println!(
        "{}: {} en {} archivos y {} carpetas, {} sin acceso, {:.2} s",
        root.display(),
        fmt::size(tree.size),
        tree.files,
        progress.dirs.load(Relaxed),
        progress.errors.load(Relaxed),
        elapsed.as_secs_f64()
    );
    for entry in tree.top_files(root, limit) {
        println!("{:>10}  {}", fmt::size(entry.size), entry.path.display());
    }
}

fn run_tui(terminal: &mut DefaultTerminal, root: PathBuf) -> std::io::Result<()> {
    let mut previous_cwd = Vec::new();
    loop {
        let Some((tree, stats)) = scan_with_ui(terminal, &root)? else {
            return Ok(());
        };
        let mut app = App::new(root.clone(), tree, stats, Protection::from_env());
        restore_position(&mut app, previous_cwd);
        browse(terminal, &mut app)?;
        if !app.rescan {
            return Ok(());
        }
        previous_cwd = std::mem::take(&mut app.cwd);
    }
}

/// Tras reescanear se vuelve a la carpeta en la que estabas, o a la más cercana que siga existiendo.
fn restore_position(app: &mut App, mut cwd: Vec<Box<std::ffi::OsStr>>) {
    while !cwd.is_empty() && app.root.descend(&cwd).is_none() {
        cwd.pop();
    }
    app.cwd = cwd;
}

/// Escanea en segundo plano mostrando el avance. `None` si se cancela.
fn scan_with_ui(
    terminal: &mut DefaultTerminal,
    root: &Path,
) -> std::io::Result<Option<(Node, ScanStats)>> {
    let progress = Arc::new(Progress::default());
    let start = Instant::now();
    // El avance sólo se puede estimar si la raíz abarca el volumen completo.
    let used = root
        .parent()
        .is_none()
        .then(|| volume_space(root))
        .flatten()
        .map(|(total, free)| total.saturating_sub(free));

    let worker = {
        let (root, progress) = (root.to_path_buf(), progress.clone());
        thread::Builder::new()
            .name("scan".into())
            .stack_size(optidisk::DEEP_STACK)
            .spawn(move || scan(&root, &progress))?
    };

    while !worker.is_finished() {
        terminal.draw(|frame| ui::draw_scanning(frame, root, &progress, start.elapsed(), used))?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()? {
                let ctrl_c = key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL);
                let wants_out = matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) || ctrl_c;
                if key.kind != KeyEventKind::Release && wants_out {
                    progress.cancel.store(true, Relaxed);
                    return Ok(None);
                }
            }
    }

    let tree = worker
        .join()
        .map_err(|_| std::io::Error::other("el escaneo falló"))?;
    let stats = ScanStats {
        elapsed: start.elapsed(),
        dirs: progress.dirs.load(Relaxed),
        errors: progress.errors.load(Relaxed),
    };
    Ok(Some((tree, stats)))
}

fn browse(terminal: &mut DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    while !app.quit && !app.rescan {
        app.tick();
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(Duration::from_millis(100))? {
            // En Windows cada pulsación también genera un evento al soltar la tecla.
            if let Event::Key(key) = event::read()?
                && key.kind != KeyEventKind::Release {
                    app.handle_key(key);
                }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Options>, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_arguments() {
        let options = parse(&["D:\\x", "-t", "4", "--print", "20"]).unwrap().unwrap();
        assert_eq!(options.root, Path::new("D:\\x"));
        assert_eq!(options.threads, 4);
        assert_eq!(options.print, Some(20));
        assert!(parse(&["--help"]).unwrap().is_none());
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["a", "b"]).is_err());
        assert!(parse(&["-t"]).is_err());
        assert!(parse(&["-t", "x"]).is_err());
    }

    #[test]
    fn default_root_is_a_directory() {
        assert!(default_root().is_dir());
    }

    #[test]
    fn rejects_roots_that_are_not_directories() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        assert!(prepare_root(&file).is_err());
        assert!(prepare_root(&dir.path().join("nope")).is_err());
        assert!(prepare_root(dir.path()).is_ok());
    }
}
