//! Inspection roots that an operator names.

use std::path::{Component, Path, PathBuf};

use crate::model::RootKind;
use crate::platform::RootSpec;

/// Resolves explicit roots in their given order. A relative path resolves
/// against `cwd`, and each root receives the identifier `explicit.N`.
pub fn explicit_roots(paths: &[PathBuf], cwd: &Path) -> Vec<RootSpec> {
    paths
        .iter()
        .enumerate()
        .map(|(index, path)| RootSpec {
            id: format!("explicit.{}", index + 1),
            kind: RootKind::Unclassified,
            path: normalize(&cwd.join(path)),
            anchor: None,
        })
        .collect()
}

/// Removes `.` components and resolves `..` components lexically.
///
/// Like the scan, lexical resolution follows no symbolic link.
pub fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{explicit_roots, normalize};

    #[test]
    fn normalizes_lexically() {
        assert_eq!(normalize(Path::new("/a/./b/../c/")), Path::new("/a/c"));
        assert_eq!(normalize(Path::new("/../a")), Path::new("/a"));
        assert_eq!(
            normalize(Path::new("/tmp/with space/x")),
            Path::new("/tmp/with space/x")
        );
    }

    #[test]
    fn resolves_relative_roots_against_the_working_directory() {
        let roots = explicit_roots(
            &[PathBuf::from("/abs"), PathBuf::from("rel/../x")],
            Path::new("/work"),
        );
        assert_eq!(roots[0].id, "explicit.1");
        assert_eq!(roots[0].path, Path::new("/abs"));
        assert_eq!(roots[1].id, "explicit.2");
        assert_eq!(roots[1].path, Path::new("/work/x"));
        assert!(roots.iter().all(|root| root.anchor.is_none()));
    }
}
