//! Recorrido paralelo del disco.
//!
//! En Windows `read_dir` devuelve tamaño, fechas y atributos en la propia
//! enumeración, así que no hace falta una llamada por archivo: el coste es una
//! enumeración por carpeta y `rayon` las reparte entre los hilos.

use crate::model::Node;
use rayon::prelude::*;
use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::UNIX_EPOCH;

/// Contadores compartidos con la interfaz durante el escaneo.
#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    pub cancel: AtomicBool,
    pub current: Mutex<PathBuf>,
}

impl Progress {
    fn cancelled(&self) -> bool {
        self.cancel.load(Relaxed)
    }

    fn entered(&self, path: &Path) {
        let dirs = self.dirs.fetch_add(1, Relaxed);
        // La ruta en curso es sólo informativa: no merece contención por carpeta.
        if dirs.is_multiple_of(64)
            && let Ok(mut current) = self.current.try_lock() {
                current.clear();
                current.push(path);
            }
    }
}

/// Mide todo lo que hay bajo `root`. El nombre del nodo raíz es la ruta completa.
pub fn scan(root: &Path, progress: &Progress) -> Node {
    walk_dir(root, root.as_os_str().into(), 0, progress)
}

/// Vuelve a medir una ruta tras un borrado que pudo quedar a medias.
/// Devuelve `None` si ya no existe.
pub fn rescan_entry(path: &Path) -> Option<Node> {
    let meta = fs::symlink_metadata(path).ok()?;
    let name: Box<OsStr> = path.file_name().unwrap_or(path.as_os_str()).into();
    let file_type = meta.file_type();
    Some(if file_type.is_symlink() {
        Node::link(name, modified_secs(&meta))
    } else if file_type.is_dir() {
        walk_dir(path, name, modified_secs(&meta), &Progress::default())
    } else {
        let parent = path.parent().unwrap_or(Path::new(""));
        Node::file(name.clone(), disk_size(&meta, parent, &name), modified_secs(&meta))
    })
}

/// Archivos y bytes que quedan bajo `path`, sin construir árbol ni usar el pool de
/// hilos: se llama durante un borrado para medir su avance y no debe competir con él.
/// Devuelve `None` si `stop` se activa a medias. Lo que desaparece mientras tanto se ignora.
pub fn measure(path: &Path, stop: &AtomicBool) -> Option<(u64, u64)> {
    let (mut files, mut bytes) = (0, 0);
    let mut pending = vec![path.to_path_buf()];
    while let Some(dir) = pending.pop() {
        if stop.load(Relaxed) {
            return None;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if let Ok(meta) = entry.metadata() {
                files += 1;
                bytes += disk_size(&meta, &dir, &entry.file_name());
            }
        }
    }
    Some((files, bytes))
}

fn walk_dir(path: &Path, name: Box<OsStr>, modified: u64, progress: &Progress) -> Node {
    let mut node = Node::dir(name, modified);
    if progress.cancelled() {
        return node;
    }
    progress.entered(path);

    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => {
            progress.errors.fetch_add(1, Relaxed);
            node.denied = true;
            node.incomplete = true;
            return node;
        }
    };

    let mut subdirs: Vec<(Box<OsStr>, u64)> = Vec::new();
    let (mut bytes, mut files) = (0, 0);
    for entry in entries {
        let read = entry.and_then(|e| Ok((e.file_name(), e.file_type()?, e.metadata()?)));
        let Ok((file_name, file_type, meta)) = read else {
            progress.errors.fetch_add(1, Relaxed);
            node.denied = true;
            continue;
        };
        let name = file_name.into_boxed_os_str();
        let modified = modified_secs(&meta);
        // `is_symlink` también es cierto en junctions y puntos de montaje, pero no
        // en las carpetas de OneDrive, que son reparse points que sí hay que medir.
        if file_type.is_symlink() {
            node.children.push(Node::link(name, modified));
        } else if file_type.is_dir() {
            subdirs.push((name, modified));
        } else {
            let size = disk_size(&meta, path, &name);
            bytes += size;
            files += 1;
            node.children.push(Node::file(name, size, modified));
        }
    }
    progress.files.fetch_add(files, Relaxed);
    progress.bytes.fetch_add(bytes, Relaxed);

    let dirs: Vec<Node> = subdirs
        .into_par_iter()
        .map(|(name, modified)| {
            let child = path.join(&*name);
            walk_dir(&child, name, modified, progress)
        })
        .collect();
    node.children.extend(dirs);
    node.recompute();
    node
}

fn modified_secs(meta: &Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

#[cfg(windows)]
fn disk_size(meta: &Metadata, dir: &Path, name: &OsStr) -> u64 {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
        FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_SPARSE_FILE,
    };
    let attributes = meta.file_attributes();
    // Marcadores de OneDrive: anuncian su tamaño en la nube, pero no ocupan disco.
    // No se tocan para no arriesgar una descarga.
    let cloud_only = FILE_ATTRIBUTE_OFFLINE
        | FILE_ATTRIBUTE_RECALL_ON_OPEN
        | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
    if attributes & cloud_only != 0 {
        return 0;
    }
    if attributes & (FILE_ATTRIBUTE_COMPRESSED | FILE_ATTRIBUTE_SPARSE_FILE) != 0 {
        return allocated_size(&dir.join(name)).unwrap_or(meta.len());
    }
    meta.len()
}

#[cfg(not(windows))]
fn disk_size(meta: &Metadata, _dir: &Path, _name: &OsStr) -> u64 {
    meta.len()
}

#[cfg(windows)]
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Bytes realmente asignados de un archivo comprimido o disperso.
#[cfg(windows)]
fn allocated_size(path: &Path) -> Option<u64> {
    use windows_sys::Win32::Foundation::{GetLastError, NO_ERROR};
    use windows_sys::Win32::Storage::FileSystem::{GetCompressedFileSizeW, INVALID_FILE_SIZE};
    // Esta API, a diferencia de `std::fs`, no alarga las rutas por su cuenta.
    let long_path;
    let path = if path.as_os_str().len() >= 260 && path.has_root() {
        long_path = Path::new(r"\\?\").join(path);
        &long_path
    } else {
        path
    };
    let name = wide(path);
    let mut high = 0u32;
    // SAFETY: `name` termina en NUL y ambos punteros viven durante la llamada.
    let low = unsafe { GetCompressedFileSizeW(name.as_ptr(), &mut high) };
    // SAFETY: GetLastError no tiene precondiciones.
    if low == INVALID_FILE_SIZE && unsafe { GetLastError() } != NO_ERROR {
        return None;
    }
    Some(u64::from(high) << 32 | u64::from(low))
}

/// Capacidad y espacio libre del volumen de `path`: `(total, libre)`.
#[cfg(windows)]
pub fn volume_space(path: &Path) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let name = wide(path);
    let (mut available, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `name` termina en NUL y los punteros de salida viven durante la llamada.
    let ok = unsafe { GetDiskFreeSpaceExW(name.as_ptr(), &mut available, &mut total, &mut free) };
    (ok != 0).then_some((total, free))
}

#[cfg(not(windows))]
pub fn volume_space(_path: &Path) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;
    use std::fs;

    #[test]
    fn measures_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("one"), vec![0; 100]).unwrap();
        fs::write(dir.path().join("a/two"), vec![0; 200]).unwrap();
        fs::write(dir.path().join("a/b/three"), vec![0; 300]).unwrap();

        let progress = Progress::default();
        let root = scan(dir.path(), &progress);

        assert_eq!(root.size, 600);
        assert_eq!(root.files, 3);
        assert_eq!(&*root.children[0].name, OsStr::new("a"));
        assert_eq!(root.children[0].size, 500);
        assert_eq!(progress.files.load(Relaxed), 3);
        assert_eq!(progress.bytes.load(Relaxed), 600);
        assert_eq!(progress.dirs.load(Relaxed), 3);
        assert_eq!(progress.errors.load(Relaxed), 0);
    }

    #[test]
    fn a_missing_root_is_reported_as_denied() {
        let dir = tempfile::tempdir().unwrap();
        let progress = Progress::default();
        let root = scan(&dir.path().join("nope"), &progress);
        assert!(root.denied && root.incomplete);
        assert_eq!(progress.errors.load(Relaxed), 1);
    }

    #[test]
    fn rescan_entry_handles_each_case() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        fs::write(&file, vec![0; 42]).unwrap();
        assert_eq!(rescan_entry(&file).unwrap().size, 42);
        assert_eq!(rescan_entry(&file).unwrap().kind, Kind::File);
        assert_eq!(rescan_entry(dir.path()).unwrap().size, 42);
        assert!(rescan_entry(&dir.path().join("nope")).is_none());
    }

    #[test]
    fn measure_counts_what_remains_and_can_be_stopped() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("one"), vec![0; 100]).unwrap();
        fs::write(dir.path().join("a/b/two"), vec![0; 200]).unwrap();

        let go = AtomicBool::new(false);
        assert_eq!(measure(dir.path(), &go), Some((2, 300)));
        assert_eq!(measure(&dir.path().join("nope"), &go), Some((0, 0)));
        assert_eq!(measure(dir.path(), &AtomicBool::new(true)), None);
    }

    #[test]
    fn cancelling_stops_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f"), b"x").unwrap();
        let progress = Progress::default();
        progress.cancel.store(true, Relaxed);
        assert_eq!(scan(dir.path(), &progress).size, 0);
    }

    #[cfg(windows)]
    #[test]
    fn volume_space_reports_the_drive() {
        let (total, free) = volume_space(Path::new(r"C:\")).unwrap();
        assert!(total > 0 && free <= total);
    }

    /// Una junction no debe medirse: su destino se contaría dos veces.
    #[cfg(windows)]
    #[test]
    fn junctions_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("data"), vec![0; 1000]).unwrap();
        let link = dir.path().join("link");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());

        let root = scan(dir.path(), &Progress::default());

        assert_eq!(root.size, 1000);
        let node = root.child(OsStr::new("link")).unwrap();
        assert_eq!(node.kind, Kind::Link);
        assert_eq!(node.size, 0);
    }
}
