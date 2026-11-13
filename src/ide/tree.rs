//! The explorer's model: a lazily loaded directory tree (useFileTree in the
//! Electron app). A directory is read when it is expanded; refreshing swaps
//! in a new listing and keeps what's loaded below it. Only `.DS_Store` is hidden.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

#[derive(Default)]
pub struct FileTree {
    pub root: PathBuf,
    children: HashMap<PathBuf, Vec<Node>>,
    pub expanded: BTreeSet<PathBuf>,
    pub selected: BTreeSet<PathBuf>,
    pub focused: Option<PathBuf>,
}

/// One visible row: its depth under the root and the node.
pub type Row = (usize, Node);

pub fn read_dir(dir: &Path) -> std::io::Result<Vec<Node>> {
    let mut out: Vec<Node> = std::fs::read_dir(dir)?
        .flatten()
        .filter(|e| e.file_name() != ".DS_Store")
        .map(|e| {
            let path = e.path();
            // Follow symlinks for "is this a folder", like the explorer shows them.
            let is_dir = std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
            Node { name: e.file_name().to_string_lossy().into_owned(), path, is_dir }
        })
        .collect();
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

impl FileTree {
    pub fn new(root: PathBuf) -> Self {
        let mut t = Self { root, ..Default::default() };
        t.load(&t.root.clone());
        t
    }

    /// Read a directory (once); its ancestors first, so it can show.
    pub fn load(&mut self, dir: &Path) {
        if self.children.contains_key(dir) {
            return;
        }
        if dir != self.root
            && let Some(parent) = dir.parent()
            && dir.starts_with(&self.root)
        {
            self.load(parent);
        }
        if let Ok(list) = read_dir(dir) {
            self.children.insert(dir.to_owned(), list);
        }
    }

    /// Re-read a loaded directory; folders that vanished drop their loaded subtrees.
    pub fn refresh(&mut self, dir: &Path) {
        if !self.children.contains_key(dir) {
            return;
        }
        match read_dir(dir) {
            Ok(list) => {
                let keep: BTreeSet<&PathBuf> = list.iter().map(|n| &n.path).collect();
                let gone: Vec<PathBuf> = self.children[dir].iter().filter(|n| !keep.contains(&n.path)).map(|n| n.path.clone()).collect();
                for g in gone {
                    self.forget(&g);
                }
                self.children.insert(dir.to_owned(), list);
            }
            Err(_) => self.forget(dir),
        }
    }

    fn forget(&mut self, path: &Path) {
        self.children.retain(|k, _| !k.starts_with(path));
        self.expanded.retain(|k| !k.starts_with(path));
        self.selected.retain(|k| !k.starts_with(path));
        if self.focused.as_ref().is_some_and(|f| f.starts_with(path)) {
            self.focused = None;
        }
    }

    /// Back to just the root, reloaded (the header's Refresh).
    pub fn refresh_all(&mut self) {
        let expanded: Vec<PathBuf> = self.expanded.iter().cloned().collect();
        self.children.clear();
        self.load(&self.root.clone());
        for dir in expanded {
            if dir.is_dir() {
                self.load(&dir);
            } else {
                self.expanded.remove(&dir);
            }
        }
    }

    pub fn toggle(&mut self, dir: &Path) {
        if !self.expanded.remove(dir) {
            self.load(dir);
            self.expanded.insert(dir.to_owned());
        }
    }

    pub fn expand(&mut self, dir: &Path) {
        self.load(dir);
        self.expanded.insert(dir.to_owned());
    }

    /// The rows as drawn: children of expanded folders, depth-first.
    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        self.push_rows(&self.root, 0, &mut out);
        out
    }

    fn push_rows(&self, dir: &Path, depth: usize, out: &mut Vec<Row>) {
        let Some(list) = self.children.get(dir) else { return };
        for n in list {
            out.push((depth, n.clone()));
            if n.is_dir && self.expanded.contains(&n.path) {
                self.push_rows(&n.path, depth + 1, out);
            }
        }
    }
}

/// Whether a watcher event under this path is worth a refresh (the Electron
/// app skips dot-segments and the usual build/dependency folders).
pub fn watched(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).map(|rel| {
        !rel.components().any(|c| {
            let s = c.as_os_str().to_string_lossy();
            s.starts_with('.') || s == "node_modules" || s == "__pycache__" || s == "target"
        })
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/inner")).unwrap();
        std::fs::write(d.path().join("src/main.rs"), "").unwrap();
        std::fs::write(d.path().join("B.txt"), "").unwrap();
        std::fs::write(d.path().join("a.txt"), "").unwrap();
        std::fs::write(d.path().join(".DS_Store"), "").unwrap();
        std::fs::write(d.path().join(".env"), "").unwrap();
        d
    }

    #[test]
    fn lists_folders_first_and_hides_only_ds_store() {
        let d = fixture();
        let t = FileTree::new(d.path().to_owned());
        let names: Vec<String> = t.rows().into_iter().map(|(_, n)| n.name).collect();
        assert_eq!(names, ["src", ".env", "a.txt", "B.txt"]);
    }

    #[test]
    fn expanding_shows_children_with_depth() {
        let d = fixture();
        let mut t = FileTree::new(d.path().to_owned());
        t.toggle(&d.path().join("src"));
        let rows: Vec<(usize, String)> = t.rows().into_iter().map(|(depth, n)| (depth, n.name)).collect();
        assert_eq!(rows[..3], [(0, "src".into()), (1, "inner".into()), (1, "main.rs".into())]);
        t.toggle(&d.path().join("src"));
        assert_eq!(t.rows().len(), 4);
    }

    #[test]
    fn refresh_picks_up_changes_and_forgets_deleted_folders() {
        let d = fixture();
        let mut t = FileTree::new(d.path().to_owned());
        t.expand(&d.path().join("src"));
        std::fs::write(d.path().join("new.txt"), "").unwrap();
        std::fs::remove_dir_all(d.path().join("src")).unwrap();
        t.refresh(d.path());
        let names: Vec<String> = t.rows().into_iter().map(|(_, n)| n.name).collect();
        assert_eq!(names, [".env", "a.txt", "B.txt", "new.txt"]);
        assert!(t.expanded.is_empty());
    }

    #[test]
    fn watcher_filter() {
        let root = Path::new("/p");
        assert!(watched(root, Path::new("/p/src/a.rs")));
        assert!(!watched(root, Path::new("/p/.git/index")));
        assert!(!watched(root, Path::new("/p/node_modules/x/y.js")));
        assert!(!watched(root, Path::new("/elsewhere/a.rs")));
    }
}
