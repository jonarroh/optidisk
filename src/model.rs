//! Árbol en memoria con los tamaños agregados del disco.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// Symlink o junction: pesa 0 y nunca se sigue, ni al medir ni al borrar.
    Link,
}

/// Los nombres son `OsStr`, no `String`: en Windows un nombre puede no ser
/// Unicode válido y una conversión con pérdida borraría (o no encontraría) otra ruta.
#[derive(Debug, Clone)]
pub struct Node {
    pub name: Box<OsStr>,
    pub kind: Kind,
    /// Bytes que ocupa en disco; en carpetas, la suma de su contenido.
    pub size: u64,
    /// Archivos contenidos (1 en un archivo).
    pub files: u64,
    /// Segundos Unix de la última modificación; en carpetas, la más reciente del contenido.
    pub modified: u64,
    /// No se pudo leer esta carpeta completa.
    pub denied: bool,
    /// Esta carpeta o alguna descendiente no se pudo leer: el tamaño es un mínimo.
    pub incomplete: bool,
    /// Ordenados de mayor a menor tamaño.
    pub children: Vec<Node>,
}

impl Node {
    pub fn file(name: impl Into<Box<OsStr>>, size: u64, modified: u64) -> Self {
        Self::leaf(name, Kind::File, size, 1, modified)
    }

    pub fn link(name: impl Into<Box<OsStr>>, modified: u64) -> Self {
        Self::leaf(name, Kind::Link, 0, 0, modified)
    }

    pub fn dir(name: impl Into<Box<OsStr>>, modified: u64) -> Self {
        Self::leaf(name, Kind::Dir, 0, 0, modified)
    }

    fn leaf(
        name: impl Into<Box<OsStr>>,
        kind: Kind,
        size: u64,
        files: u64,
        modified: u64,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            size,
            files,
            modified,
            denied: false,
            incomplete: false,
            children: Vec::new(),
        }
    }

    /// Recalcula los agregados a partir de los hijos y los reordena.
    pub fn recompute(&mut self) {
        self.size = self.children.iter().map(|c| c.size).sum();
        self.files = self.children.iter().map(|c| c.files).sum();
        if let Some(newest) = self.children.iter().map(|c| c.modified).max() {
            self.modified = newest;
        }
        self.incomplete = self.denied || self.children.iter().any(|c| c.incomplete);
        self.children
            .sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    }

    pub fn child(&self, name: &OsStr) -> Option<&Node> {
        self.children.iter().find(|c| &*c.name == name)
    }

    /// Nodo en la ruta dada, relativa a este nodo.
    pub fn descend<S: AsRef<OsStr>>(&self, names: &[S]) -> Option<&Node> {
        names
            .iter()
            .try_fold(self, |node, name| node.child(name.as_ref()))
    }

    /// Sustituye (`Some`) o quita (`None`) el nodo en la ruta dada y actualiza los
    /// agregados de todos sus ancestros. Devuelve `false` si la ruta no existe.
    pub fn update_at<S: AsRef<OsStr>>(&mut self, names: &[S], replacement: Option<Node>) -> bool {
        let Some((first, rest)) = names.split_first() else {
            return false;
        };
        let first = first.as_ref();
        let Some(index) = self.children.iter().position(|c| &*c.name == first) else {
            return false;
        };
        if rest.is_empty() {
            match replacement {
                Some(node) => self.children[index] = node,
                None => {
                    self.children.remove(index);
                }
            }
        } else if !self.children[index].update_at(rest, replacement) {
            return false;
        }
        self.recompute();
        true
    }

    /// Los `limit` archivos más grandes del árbol, de mayor a menor.
    pub fn top_files(&self, base: &Path, limit: usize) -> Vec<TopEntry> {
        let mut heap = BinaryHeap::with_capacity(limit + 1);
        let mut trail = Vec::new();
        collect_top(self, base, limit, &mut trail, &mut heap);
        heap.into_sorted_vec()
            .into_iter()
            .map(|Reverse(entry)| entry)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TopEntry {
    pub size: u64,
    pub path: PathBuf,
    pub modified: u64,
}

fn collect_top<'a>(
    node: &'a Node,
    base: &Path,
    limit: usize,
    trail: &mut Vec<&'a OsStr>,
    heap: &mut BinaryHeap<Reverse<TopEntry>>,
) {
    for child in &node.children {
        match child.kind {
            Kind::File => {
                let qualifies = heap.len() < limit
                    || heap.peek().is_some_and(|min| child.size > min.0.size);
                if qualifies && limit > 0 {
                    let mut path = base.to_path_buf();
                    path.extend(trail.iter());
                    path.push(&*child.name);
                    heap.push(Reverse(TopEntry {
                        size: child.size,
                        path,
                        modified: child.modified,
                    }));
                    if heap.len() > limit {
                        heap.pop();
                    }
                }
            }
            Kind::Dir => {
                trail.push(&child.name);
                collect_top(child, base, limit, trail, heap);
                trail.pop();
            }
            Kind::Link => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Node {
        let mut sub = Node::dir(OsStr::new("sub"), 5);
        sub.children.push(Node::file(OsStr::new("big"), 100, 50));
        sub.children.push(Node::file(OsStr::new("small"), 10, 40));
        sub.recompute();
        let mut root = Node::dir(OsStr::new("root"), 1);
        root.children.push(Node::file(OsStr::new("mid"), 50, 30));
        root.children.push(sub);
        root.children.push(Node::link(OsStr::new("link"), 9));
        root.recompute();
        root
    }

    #[test]
    fn aggregates_and_order() {
        let root = sample();
        assert_eq!(root.size, 160);
        assert_eq!(root.files, 3);
        assert_eq!(root.modified, 50);
        let names: Vec<_> = root.children.iter().map(|c| c.name.to_str().unwrap()).collect();
        assert_eq!(names, ["sub", "mid", "link"]);
    }

    #[test]
    fn removing_updates_every_ancestor() {
        let mut root = sample();
        assert!(root.update_at(&["sub", "big"], None));
        assert_eq!(root.size, 60);
        assert_eq!(root.files, 2);
        assert_eq!(root.modified, 40);
        // Ahora «mid» (50) supera a «sub» (10).
        assert_eq!(&*root.children[0].name, OsStr::new("mid"));
    }

    #[test]
    fn replacing_a_partially_deleted_subtree() {
        let mut root = sample();
        let mut partial = Node::dir(OsStr::new("sub"), 5);
        partial.children.push(Node::file(OsStr::new("small"), 10, 40));
        partial.recompute();
        assert!(root.update_at(&["sub"], Some(partial)));
        assert_eq!(root.size, 60);
    }

    #[test]
    fn unknown_paths_change_nothing() {
        let mut root = sample();
        assert!(!root.update_at(&["sub", "nope"], None));
        assert!(!root.update_at::<&str>(&[], None));
        assert_eq!(root.size, 160);
    }

    #[test]
    fn denied_propagates_and_clears() {
        let mut root = sample();
        let mut blocked = Node::dir(OsStr::new("blocked"), 1);
        blocked.denied = true;
        blocked.incomplete = true;
        root.children.push(blocked);
        root.recompute();
        assert!(root.incomplete);
        root.update_at(&["blocked"], None);
        assert!(!root.incomplete);
    }

    #[test]
    fn top_files_are_the_largest_and_skip_links() {
        let root = sample();
        let top = root.top_files(Path::new("/base"), 2);
        let sizes: Vec<_> = top.iter().map(|e| e.size).collect();
        assert_eq!(sizes, [100, 50]);
        assert_eq!(top[0].path, Path::new("/base").join("sub").join("big"));
        assert!(root.top_files(Path::new("/base"), 0).is_empty());
    }
}
