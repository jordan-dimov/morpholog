//! A programme in parts: a `.parts` manifest lists its `.morph` files, one relative path per
//! line, in programme order, and the runtime reads them as one text. Blank lines and `--`
//! comment lines are allowed.

use morpholog_surface::SourceText;
use std::path::{Component, Path, PathBuf};

/// Whether a path names a manifest rather than a `.morph` file.
pub(crate) fn is_manifest(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "parts")
}

/// Why a manifest names no programme, with the manifest line it is about.
#[derive(Debug)]
pub(crate) enum Refusal {
    Unreadable(std::io::Error),
    NoParts,
    Absolute {
        line: usize,
        path: String,
    },
    Parent {
        line: usize,
        path: String,
    },
    Nested {
        line: usize,
        path: String,
    },
    NotMorph {
        line: usize,
        path: String,
    },
    Missing {
        line: usize,
        path: String,
        error: std::io::Error,
    },
    Outside {
        line: usize,
        path: String,
    },
    Repeated {
        line: usize,
        path: String,
        first: usize,
    },
}

/// A refused manifest, named in its message.
#[derive(Debug)]
pub(crate) struct Refused {
    manifest: PathBuf,
    pub(crate) refusal: Refusal,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let manifest = self.manifest.display();
        match &self.refusal {
            Refusal::Unreadable(e) => write!(f, "read manifest {manifest}: {e}"),
            Refusal::NoParts => write!(f, "{manifest} lists no parts"),
            Refusal::Absolute { line, path } => write!(
                f,
                "{manifest}:{line}: `{path}` is absolute; a part is named relative to its manifest"
            ),
            Refusal::Parent { line, path } => write!(
                f,
                "{manifest}:{line}: `{path}` reaches out of the manifest's directory with `..`; \
                 a part lives beside or below its manifest"
            ),
            Refusal::Nested { line, path } => write!(
                f,
                "{manifest}:{line}: `{path}` is a manifest; a manifest lists `.morph` files only"
            ),
            Refusal::NotMorph { line, path } => {
                write!(f, "{manifest}:{line}: `{path}` is not a `.morph` file")
            }
            Refusal::Missing { line, path, error } => {
                write!(f, "{manifest}:{line}: read part `{path}`: {error}")
            }
            Refusal::Outside { line, path } => write!(
                f,
                "{manifest}:{line}: `{path}` resolves outside the manifest's directory; a part \
                 lives beside or below its manifest"
            ),
            Refusal::Repeated { line, path, first } => write!(
                f,
                "{manifest}:{line}: `{path}` is the file line {first} already lists"
            ),
        }
    }
}

impl std::error::Error for Refused {}

/// Read the parts a manifest lists, in order, each named by its path from the working
/// directory.
pub(crate) fn read(manifest: &Path) -> Result<SourceText, Refused> {
    let refused = |refusal| Refused {
        manifest: manifest.to_path_buf(),
        refusal,
    };
    let listing = std::fs::read_to_string(manifest).map_err(|e| refused(Refusal::Unreadable(e)))?;
    let dir = manifest.parent().unwrap_or(Path::new(""));
    let root = (if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    })
    .canonicalize()
    .map_err(|e| refused(Refusal::Unreadable(e)))?;
    let mut seen: Vec<(PathBuf, usize)> = Vec::new();
    let mut parts = Vec::new();
    for (n, raw) in listing.lines().enumerate() {
        let (line, entry) = (n + 1, raw.trim());
        if entry.is_empty() || entry.starts_with("--") {
            continue;
        }
        let path = entry.to_string();
        let named = Path::new(entry);
        if named.is_absolute() {
            return Err(refused(Refusal::Absolute { line, path }));
        }
        if named.components().any(|c| c == Component::ParentDir) {
            return Err(refused(Refusal::Parent { line, path }));
        }
        if is_manifest(named) {
            return Err(refused(Refusal::Nested { line, path }));
        }
        if named.extension().is_none_or(|e| e != "morph") {
            return Err(refused(Refusal::NotMorph { line, path }));
        }
        let file = dir.join(named);
        let canonical = match file.canonicalize() {
            Ok(c) => c,
            Err(error) => return Err(refused(Refusal::Missing { line, path, error })),
        };
        if !canonical.starts_with(&root) {
            return Err(refused(Refusal::Outside { line, path }));
        }
        if let Some(&(_, first)) = seen.iter().find(|(c, _)| *c == canonical) {
            return Err(refused(Refusal::Repeated { line, path, first }));
        }
        seen.push((canonical, line));
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(error) => return Err(refused(Refusal::Missing { line, path, error })),
        };
        parts.push((file.display().to_string(), text));
    }
    SourceText::parts(parts).ok_or_else(|| refused(Refusal::NoParts))
}

#[cfg(test)]
mod tests {
    use super::{Refusal, read};
    use std::path::Path;

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::write(dir.join(name), text).unwrap();
    }

    /// A directory holding a header part and one declaration part, and a manifest listing
    /// `listing`.
    fn manifest(listing: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.morph", "program shop\n");
        write(dir.path(), "b.morph", "predicate Stock(item: Subject)\n");
        let path = dir.path().join("shop.parts");
        std::fs::write(&path, listing).unwrap();
        (dir, path)
    }

    fn refusal(listing: &str) -> Refusal {
        let (_dir, path) = manifest(listing);
        read(&path).expect_err("refused").refusal
    }

    #[test]
    fn parts_are_read_in_listed_order_and_named_from_the_working_directory() {
        let (dir, path) = manifest("-- the shop\n\na.morph\n  b.morph  \n");
        let source = read(&path).unwrap();
        let names: Vec<&str> = source.files().collect();
        let a = dir.path().join("a.morph").display().to_string();
        let b = dir.path().join("b.morph").display().to_string();
        assert_eq!(names, [a.as_str(), b.as_str()]);
        assert_eq!(
            source.text(),
            "program shop\npredicate Stock(item: Subject)\n"
        );
    }

    #[test]
    fn each_malformed_entry_is_refused_by_name() {
        assert!(matches!(
            refusal("a.morph\n/etc/b.morph\n"),
            Refusal::Absolute { line: 2, .. }
        ));
        assert!(matches!(
            refusal("a.morph\nsub/../b.morph\n"),
            Refusal::Parent { line: 2, .. }
        ));
        assert!(matches!(
            refusal("a.morph\nother.parts\n"),
            Refusal::Nested { line: 2, .. }
        ));
        assert!(matches!(
            refusal("a.morph\nb.txt\n"),
            Refusal::NotMorph { line: 2, .. }
        ));
        assert!(matches!(
            refusal("a.morph\nc.morph\n"),
            Refusal::Missing { line: 2, .. }
        ));
        assert!(matches!(
            refusal("a.morph\nb.morph\n./b.morph\n"),
            Refusal::Repeated {
                line: 3,
                first: 2,
                ..
            }
        ));
        assert!(matches!(refusal("-- nothing yet\n\n"), Refusal::NoParts));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_judged_by_what_it_resolves_to() {
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "far.morph", "predicate Far(x: Subject)\n");
        let (dir, path) = manifest("a.morph\nb.morph\nalias.morph\n");
        std::os::unix::fs::symlink(dir.path().join("b.morph"), dir.path().join("alias.morph"))
            .unwrap();
        assert!(matches!(
            read(&path).expect_err("an alias").refusal,
            Refusal::Repeated {
                line: 3,
                first: 2,
                ..
            }
        ));
        std::fs::write(&path, "a.morph\nnear.morph\n").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("far.morph"),
            dir.path().join("near.morph"),
        )
        .unwrap();
        assert!(matches!(
            read(&path).expect_err("outside").refusal,
            Refusal::Outside { line: 2, .. }
        ));
    }
}
