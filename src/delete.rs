//! Borrado: qué rutas se pueden tocar y el trabajador que las borra en segundo plano.

use crate::model::Node;
use crate::scan::{measure, rescan_entry};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    /// A la papelera: recuperable, pero no libera espacio hasta vaciarla.
    Trash,
    /// Sin vuelta atrás.
    Permanent,
}

/// Rutas que nunca se ofrecen para borrar y avisos para las que sí, pero merecen cuidado.
#[derive(Debug, Default)]
pub struct Protection {
    /// Ni estas rutas ni sus ancestros se pueden borrar; su contenido, sí.
    roots: Vec<String>,
    /// Ni estas rutas ni nada dentro de ellas se puede borrar.
    trees: Vec<String>,
    cloud: Vec<String>,
}

const SPECIAL_FOLDERS: [&str; 2] = ["system volume information", "$recycle.bin"];
const SYSTEM_FILES: [&str; 3] = ["pagefile.sys", "hiberfil.sys", "swapfile.sys"];
const PROFILE_FOLDERS: [&str; 10] = [
    "Desktop",
    "Documents",
    "Downloads",
    "Pictures",
    "Music",
    "Videos",
    "AppData",
    r"AppData\Local",
    r"AppData\LocalLow",
    r"AppData\Roaming",
];

impl Protection {
    pub fn from_env() -> Self {
        Self::from_vars(|name| std::env::var_os(name).map(PathBuf::from))
    }

    pub fn from_vars(var: impl Fn(&str) -> Option<PathBuf>) -> Self {
        let mut roots = Vec::new();
        for name in [
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "ProgramData",
            "PUBLIC",
        ] {
            roots.extend(var(name));
        }
        if let Some(profile) = var("USERPROFILE") {
            roots.extend(PROFILE_FOLDERS.iter().map(|sub| profile.join(sub)));
            roots.push(profile);
        }
        let trees = ["SystemRoot", "windir"].iter().filter_map(|n| var(n));
        let cloud = ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"]
            .iter()
            .filter_map(|n| var(n));
        Self {
            roots: roots.iter().map(|p| normalize(p)).collect(),
            trees: trees.map(|p| normalize(&p)).collect(),
            cloud: cloud.map(|p| normalize(&p)).collect(),
        }
    }

    /// Motivo por el que `path` no se puede borrar, si lo hay.
    pub fn blocked(&self, path: &Path) -> Option<&'static str> {
        if path.parent().is_none() {
            return Some("es la raíz de una unidad");
        }
        let target = normalize(path);
        if self.trees.iter().any(|t| is_within(&target, t)) {
            return Some("es parte de Windows");
        }
        if self.roots.iter().any(|r| is_within(r, &target)) {
            return Some("es una carpeta esencial del sistema o de tu perfil");
        }
        if target.split('\\').any(|c| SPECIAL_FOLDERS.contains(&c)) {
            return Some("es una carpeta del sistema");
        }
        let file_name = target.rsplit('\\').next().unwrap_or_default();
        if SYSTEM_FILES.contains(&file_name) {
            return Some("lo gestiona Windows");
        }
        None
    }

    /// Avisos sobre consecuencias poco evidentes de borrar `path`.
    pub fn warning(&self, path: &Path) -> Option<&'static str> {
        let target = normalize(path);
        if target.ends_with(".vhdx") || target.ends_with(".vhd") {
            return Some(
                "Disco virtual (.vhdx): borrarlo destruye esa distro de WSL o los datos de Docker. \
                 Para recuperar espacio suele bastar con compactarlo.",
            );
        }
        if self.cloud.iter().any(|c| is_within(&target, c)) {
            return Some("Carpeta de OneDrive: borrar aquí también lo borra en la nube.");
        }
        None
    }
}

fn normalize(path: &Path) -> String {
    let text = path.to_string_lossy().to_lowercase().replace('/', "\\");
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    text.trim_end_matches('\\').to_string()
}

/// ¿`inner` es `outer` o está dentro de ella? Ambas normalizadas.
fn is_within(inner: &str, outer: &str) -> bool {
    inner
        .strip_prefix(outer)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'))
}

/// Quita las rutas ya cubiertas por un ancestro marcado: borrarlas aparte fallaría y
/// sumarlas contaría dos veces. `Path` ordena por componentes, así que cada
/// descendiente aparece justo después de su ancestro.
pub fn without_nested(marks: &BTreeMap<PathBuf, u64>) -> Vec<(PathBuf, u64)> {
    let mut kept: Vec<(PathBuf, u64)> = Vec::new();
    for (path, size) in marks {
        if kept.last().is_none_or(|(last, _)| !path.starts_with(last)) {
            kept.push((path.clone(), *size));
        }
    }
    kept
}

pub fn remove(path: &Path, mode: DeleteMode) -> Result<(), String> {
    match mode {
        DeleteMode::Trash => trash::delete(path).map_err(|e| e.to_string()),
        DeleteMode::Permanent => remove_permanently(path).map_err(|e| e.to_string()),
    }
}

fn remove_permanently(path: &Path) -> std::io::Result<()> {
    let file_type = fs::symlink_metadata(path)?.file_type();
    if file_type.is_symlink() {
        // Un enlace a carpeta se quita como carpeta; nunca se entra en su destino.
        fs::remove_file(path).or_else(|_| fs::remove_dir(path))
    } else if file_type.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

pub enum DeleteEvent {
    Started(PathBuf),
    /// Lo que aún queda en disco del elemento en curso.
    Remaining { files: u64, bytes: u64 },
    /// Terminó un elemento. `remaining` es lo que quedó en disco: `None` si ya no
    /// existe, o el subárbol vuelto a medir si el borrado quedó a medias.
    Finished {
        path: PathBuf,
        error: Option<String>,
        remaining: Option<Node>,
    },
    Done,
}

const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// Borra informando del avance. `remove_dir_all` no lo da, y sustituirlo por un
/// recorrido propio perdería su protección contra el cambio de una carpeta por un
/// enlace durante el borrado, así que un hilo aparte mide lo que va quedando. La
/// papelera no se mide: mueve la carpeta entera de una vez.
fn remove_reporting(path: &Path, mode: DeleteMode, tx: &Sender<DeleteEvent>) -> Result<(), String> {
    let is_dir = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir());
    if mode != DeleteMode::Permanent || !is_dir {
        return remove(path, mode);
    }
    let finished = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| report_remaining(path, &finished, tx));
        let result = remove(path, mode);
        finished.store(true, Relaxed);
        result
    })
}

fn report_remaining(path: &Path, finished: &AtomicBool, tx: &Sender<DeleteEvent>) {
    let mut wait = SAMPLE_INTERVAL;
    loop {
        // Espera en tramos cortos: al terminar el borrado no debe retrasarlo.
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            if finished.load(Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let start = Instant::now();
        let Some((files, bytes)) = measure(path, finished) else {
            return;
        };
        let _ = tx.send(DeleteEvent::Remaining { files, bytes });
        // En árboles enormes medir es caro: no más de una cuarta parte del tiempo.
        wait = SAMPLE_INTERVAL.max(start.elapsed() * 3);
    }
}

/// Borra en otro hilo para no congelar la interfaz; `cancel` detiene antes del siguiente elemento.
pub fn spawn(
    targets: Vec<PathBuf>,
    mode: DeleteMode,
    cancel: Arc<AtomicBool>,
) -> Receiver<DeleteEvent> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("delete".into())
        .stack_size(crate::DEEP_STACK)
        .spawn(move || {
            for path in targets {
                if cancel.load(Relaxed) {
                    break;
                }
                let _ = tx.send(DeleteEvent::Started(path.clone()));
                let mut error = remove_reporting(&path, mode, &tx).err();
                let remaining = rescan_entry(&path);
                if error.is_none() && remaining.is_some() {
                    error = Some("sigue existiendo tras borrarlo".into());
                }
                let _ = tx.send(DeleteEvent::Finished {
                    path,
                    error,
                    remaining,
                });
            }
            let _ = tx.send(DeleteEvent::Done);
        })
        .expect("no se pudo crear el hilo de borrado");
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protection() -> Protection {
        Protection::from_vars(|name| {
            let value = match name {
                "SystemRoot" => r"C:\Windows",
                "ProgramFiles" => r"C:\Program Files",
                "ProgramData" => r"C:\ProgramData",
                "USERPROFILE" => r"C:\Users\ana",
                "OneDrive" => r"C:\Users\ana\OneDrive",
                _ => return None,
            };
            Some(PathBuf::from(value))
        })
    }

    #[cfg(windows)]
    #[test]
    fn drive_roots_are_blocked() {
        assert!(protection().blocked(Path::new(r"C:\")).is_some());
        assert!(protection().blocked(Path::new(r"D:\")).is_some());
    }

    #[test]
    fn windows_and_its_contents_are_blocked() {
        let p = protection();
        assert!(p.blocked(Path::new(r"C:\Windows")).is_some());
        assert!(p.blocked(Path::new(r"c:\windows\System32\x.dll")).is_some());
        assert!(p.blocked(Path::new(r"C:\Windows.old")).is_none());
    }

    #[test]
    fn essential_folders_and_ancestors_are_blocked_but_contents_are_not() {
        let p = protection();
        assert!(p.blocked(Path::new(r"C:\Users")).is_some());
        assert!(p.blocked(Path::new(r"C:\Users\ana")).is_some());
        assert!(p.blocked(Path::new(r"C:\Users\ana\AppData")).is_some());
        assert!(p.blocked(Path::new(r"C:\Users\ana\AppData\Local")).is_some());
        assert!(p.blocked(Path::new(r"C:\Program Files")).is_some());
        assert!(p.blocked(Path::new(r"C:\Program Files\App")).is_none());
        assert!(p.blocked(Path::new(r"C:\Users\ana\AppData\Local\Temp")).is_none());
        assert!(p.blocked(Path::new(r"C:\Users\ana\Downloads\a.iso")).is_none());
        assert!(p.blocked(Path::new(r"C:\Users\anabel")).is_none());
    }

    #[test]
    fn system_files_and_folders_are_blocked() {
        let p = protection();
        assert!(p.blocked(Path::new(r"C:\pagefile.sys")).is_some());
        assert!(p.blocked(Path::new(r"C:\hiberfil.sys")).is_some());
        assert!(p.blocked(Path::new(r"D:\System Volume Information\x")).is_some());
        assert!(p.blocked(Path::new(r"D:\$Recycle.Bin")).is_some());
        assert!(p.blocked(Path::new(r"D:\juegos")).is_none());
    }

    #[test]
    fn warnings_cover_virtual_disks_and_onedrive() {
        let p = protection();
        assert!(p.warning(Path::new(r"C:\x\docker_data.VHDX")).is_some());
        assert!(p.warning(Path::new(r"C:\Users\ana\OneDrive\foto.jpg")).is_some());
        assert!(p.warning(Path::new(r"C:\Users\ana\OneDriveTemp")).is_none());
        assert!(p.warning(Path::new(r"C:\x\a.iso")).is_none());
    }

    #[test]
    fn nested_marks_are_dropped() {
        let marks = BTreeMap::from([
            (PathBuf::from("a/b"), 10),
            (PathBuf::from("a/b/c"), 4),
            (PathBuf::from("a/b.txt"), 2),
            (PathBuf::from("a/bc"), 3),
        ]);
        let kept: Vec<_> = without_nested(&marks).into_iter().map(|(p, _)| p).collect();
        assert_eq!(
            kept,
            [
                PathBuf::from("a/b"),
                PathBuf::from("a/b.txt"),
                PathBuf::from("a/bc")
            ]
        );
    }

    #[test]
    fn permanent_removes_trees_and_read_only_files() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree/inner");
        fs::create_dir_all(&tree).unwrap();
        let locked = tree.join("locked");
        fs::write(&locked, b"x").unwrap();
        let mut permissions = fs::metadata(&locked).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&locked, permissions).unwrap();

        let file = dir.path().join("lone");
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&file, {
            let mut p = fs::metadata(&file).unwrap().permissions();
            p.set_readonly(true);
            p
        })
        .unwrap();

        remove(&file, DeleteMode::Permanent).unwrap();
        remove(&dir.path().join("tree"), DeleteMode::Permanent).unwrap();
        assert!(!file.exists());
        assert!(!dir.path().join("tree").exists());
    }

    #[test]
    fn missing_paths_report_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(remove(&dir.path().join("nope"), DeleteMode::Permanent).is_err());
    }

    #[test]
    fn worker_reports_each_outcome_and_honours_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::write(&a, b"x").unwrap();

        let rx = spawn(
            vec![a.clone(), b],
            DeleteMode::Permanent,
            Arc::new(AtomicBool::new(false)),
        );
        let outcomes: Vec<_> = rx
            .iter()
            .filter_map(|e| match e {
                DeleteEvent::Finished {
                    error, remaining, ..
                } => Some((error.is_some(), remaining.is_some())),
                _ => None,
            })
            .collect();
        assert_eq!(outcomes, [(false, false), (true, false)]);
        assert!(!a.exists());

        let keep = dir.path().join("keep");
        fs::write(&keep, b"x").unwrap();
        let rx = spawn(
            vec![keep.clone()],
            DeleteMode::Permanent,
            Arc::new(AtomicBool::new(true)),
        );
        assert!(matches!(rx.recv().unwrap(), DeleteEvent::Done));
        assert!(keep.exists());
    }

    /// Medir mientras se borra no debe hacer fallar el borrado.
    #[test]
    fn measuring_does_not_disturb_a_large_delete() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        for d in 0..60 {
            let sub = tree.join(format!("d{d}/nested"));
            fs::create_dir_all(&sub).unwrap();
            for f in 0..100 {
                fs::write(sub.join(format!("f{f}")), [0u8; 64]).unwrap();
            }
        }

        let rx = spawn(
            vec![tree.clone()],
            DeleteMode::Permanent,
            Arc::new(AtomicBool::new(false)),
        );
        for event in rx {
            match event {
                DeleteEvent::Remaining { files, bytes } => {
                    assert!(files <= 6000 && bytes <= 6000 * 64);
                }
                DeleteEvent::Finished {
                    error, remaining, ..
                } => {
                    assert_eq!(error, None);
                    assert!(remaining.is_none());
                }
                _ => {}
            }
        }
        assert!(!tree.exists());
    }

    #[cfg(windows)]
    fn make_junction(link: &Path, target: &Path) {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Borrar una junction debe quitar sólo el enlace, jamás el contenido de su destino.
    #[cfg(windows)]
    #[test]
    fn permanent_delete_of_a_junction_keeps_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("precious"), b"x").unwrap();
        let link = dir.path().join("link");
        make_junction(&link, &target);

        remove(&link, DeleteMode::Permanent).unwrap();

        assert!(fs::symlink_metadata(&link).is_err());
        assert!(target.join("precious").exists());
    }

    /// Lo mismo cuando la junction está dentro de una carpeta borrada recursivamente.
    #[cfg(windows)]
    #[test]
    fn permanent_delete_does_not_cross_a_nested_junction() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("precious"), b"x").unwrap();
        let tree = dir.path().join("tree");
        fs::create_dir(&tree).unwrap();
        make_junction(&tree.join("link"), &target);

        remove(&tree, DeleteMode::Permanent).unwrap();

        assert!(!tree.exists());
        assert!(target.join("precious").exists());
    }

    /// Usa la papelera real y purga sólo lo que este test puso en ella.
    #[cfg(windows)]
    #[test]
    fn trash_moves_items_and_keeps_junction_targets() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let file = dir.path().join("optidisk-test-file");
        fs::write(&file, b"x").unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("precious"), b"x").unwrap();
        let link = dir.path().join("optidisk-test-link");
        make_junction(&link, &target);

        let result = remove(&file, DeleteMode::Trash).and(remove(&link, DeleteMode::Trash));

        let mine: Vec<_> = trash::os_limited::list()
            .unwrap()
            .into_iter()
            .filter(|item| {
                PathBuf::from(&item.original_parent)
                    .canonicalize()
                    .is_ok_and(|parent| parent == canonical)
                    || item.original_parent.starts_with(dir.path())
            })
            .collect();
        let trashed = mine.len();
        trash::os_limited::purge_all(mine).unwrap();

        result.unwrap();
        assert_eq!(trashed, 2);
        assert!(!file.exists());
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(target.join("precious").exists());
    }
}
