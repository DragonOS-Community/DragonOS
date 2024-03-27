#![forbid(unsafe_code)]

//! `clean-path` is a safe fork of the
//! [`path-clean`](https://crates.io/crates/path-clean) crate.
//!
//! # About
//!
//! This fork aims to provide the same utility as
//! [`path-clean`](https://crates.io/crates/path-clean), without using unsafe. Additionally, the api
//! is improved ([`clean`] takes `AsRef<Path>` instead of just `&str`) and `Clean` is implemented on
//! `Path` in addition to just `PathBuf`.
//!
//! The main cleaning procedure is implemented using the methods provided by `PathBuf`, thus it should
//! bring portability benefits over [`path-clean`](https://crates.io/crates/path-clean) w.r.t. correctly
//! handling cross-platform filepaths. However, the current implementation is not highly-optimized, so
//! if performance is top-priority, consider using [`path-clean`](https://crates.io/crates/path-clean)
//! instead.
//!
//! # Specification
//!
//! The cleaning works as follows:
//! 1. Reduce multiple slashes to a single slash.
//! 2. Eliminate `.` path name elements (the current directory).
//! 3. Eliminate `..` path name elements (the parent directory) and the non-`.` non-`..`, element that precedes them.
//! 4. Eliminate `..` elements that begin a rooted path, that is, replace `/..` by `/` at the beginning of a path.
//! 5. Leave intact `..` elements that begin a non-rooted path.
//!
//! If the result of this process is an empty string, return the
//! string `"."`, representing the current directory.
//!
//! This transformation is performed lexically, without touching the filesystem. Therefore it doesn't do
//! any symlink resolution or absolute path resolution. For more information you can see ["Getting
//! Dot-Dot Right"](https://9p.io/sys/doc/lexnames.html).
//!
//! This functionality is exposed in the [`clean`] function and [`Clean`] trait implemented for
//! [`crate::PathBuf`] and [`crate::Path`].
//!
//!
//! # Example
//!
//! ```rust
//! use path_base::{Path, PathBuf};
//! use path_base::clean_path::{clean, Clean};
//!
//! assert_eq!(clean("foo/../../bar"), PathBuf::from("../bar"));
//! assert_eq!(Path::new("hello/world/..").clean(), PathBuf::from("hello"));
//! assert_eq!(
//!     PathBuf::from("/test/../path/").clean(),
//!     PathBuf::from("/path")
//! );
//! ```

use crate::{Path, PathBuf};

/// The Clean trait implements the `clean` method.
pub trait Clean {
    fn clean(&self) -> PathBuf;
}

/// Clean implemented for PathBuf
impl Clean for PathBuf {
    fn clean(&self) -> PathBuf {
        clean(self)
    }
}

/// Clean implemented for PathBuf
impl Clean for Path {
    fn clean(&self) -> PathBuf {
        clean(self)
    }
}

/**
Clean the given path to according to a set of rules:
1. Reduce multiple slashes to a single slash.
2. Eliminate `.` path name elements (the current directory).
3. Eliminate `..` path name elements (the parent directory) and the non-`.` non-`..`, element that precedes them.
4. Eliminate `..` elements that begin a rooted path, that is, replace `/..` by `/` at the beginning of a path.
5. Leave intact `..` elements that begin a non-rooted path.

If the result of this process is an empty string, return the string `"."`, representing the current directory.

Note that symlinks and absolute paths are not resolved.

# Example

```rust
# use path_base::PathBuf;
# use path_base::clean_path::{clean, Clean};
assert_eq!(clean("foo/../../bar"), PathBuf::from("../bar"));
```
*/
pub fn clean<P: AsRef<Path>>(path: P) -> PathBuf {
    let path = path.as_ref();
    clean_internal(path)
}

/// The core implementation.
fn clean_internal(path: &Path) -> PathBuf {
    // based off of github.com/rust-lang/cargo/blob/fede83/src/cargo/util/paths.rs#L61
    use crate::Component;

    let mut components = path.components().peekable();
    let mut cleaned = if let Some(c @ Component::Prefix(..)) = components.peek().cloned() {
        components.next();
        PathBuf::from(c.as_os_str())
    } else {
        PathBuf::new()
    };

    // amount of leading parentdir components in `cleaned`
    let mut dotdots = 0;
    // amount of components in `cleaned`
    // invariant: component_count >= dotdots
    let mut component_count = 0;

    for component in components {
        match component {
            Component::Prefix(..) => unreachable!(),
            Component::RootDir => {
                cleaned.push(component.as_os_str());
                component_count += 1;
            }
            Component::CurDir => {}
            Component::ParentDir if component_count == 1 && cleaned.is_absolute() => {}
            Component::ParentDir if component_count == dotdots => {
                cleaned.push("..");
                dotdots += 1;
                component_count += 1;
            }
            Component::ParentDir => {
                cleaned.pop();
                component_count -= 1;
            }
            Component::Normal(c) => {
                cleaned.push(c);
                component_count += 1;
            }
        }
    }

    if component_count == 0 {
        cleaned.push(".");
    }

    cleaned
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{clean, Clean};
    use crate::PathBuf;

    #[test]
    fn test_empty_path_is_current_dir() {
        assert_eq!(clean(""), PathBuf::from("."));
    }

    #[test]
    fn test_clean_paths_dont_change() {
        let tests = vec![(".", "."), ("..", ".."), ("/", "/")];

        for test in tests {
            assert_eq!(
                clean(test.0),
                PathBuf::from(test.1),
                "clean({}) == {}",
                test.0,
                test.1
            );
        }
    }

    #[test]
    fn test_replace_multiple_slashes() {
        let tests = vec![
            ("/", "/"),
            ("//", "/"),
            ("///", "/"),
            (".//", "."),
            ("//..", "/"),
            ("..//", ".."),
            ("/..//", "/"),
            ("/.//./", "/"),
            ("././/./", "."),
            ("path//to///thing", "path/to/thing"),
            ("/path//to///thing", "/path/to/thing"),
        ];

        for test in tests {
            assert_eq!(
                clean(test.0),
                PathBuf::from(test.1),
                "clean({}) == {}",
                test.0,
                test.1
            );
        }
    }

    #[test]
    fn test_eliminate_current_dir() {
        let tests = vec![
            ("./", "."),
            ("/./", "/"),
            ("./test", "test"),
            ("./test/./path", "test/path"),
            ("/test/./path/", "/test/path"),
            ("test/path/.", "test/path"),
        ];

        for test in tests {
            assert_eq!(
                clean(test.0),
                PathBuf::from(test.1),
                "clean({}) == {}",
                test.0,
                test.1
            );
        }
    }

    #[test]
    fn test_eliminate_parent_dir() {
        let tests = vec![
            ("/..", "/"),
            ("/../test", "/test"),
            ("test/..", "."),
            ("test/path/..", "test"),
            ("test/../path", "path"),
            ("/test/../path", "/path"),
            ("test/path/../../", "."),
            ("test/path/../../..", ".."),
            ("/test/path/../../..", "/"),
            ("/test/path/../../../..", "/"),
            ("test/path/../../../..", "../.."),
            ("test/path/../../another/path", "another/path"),
            ("test/path/../../another/path/..", "another"),
            ("../test", "../test"),
            ("../test/", "../test"),
            ("../test/path", "../test/path"),
            ("../test/..", ".."),
        ];

        for test in tests {
            assert_eq!(
                clean(test.0),
                PathBuf::from(test.1),
                "clean({}) == {}",
                test.0,
                test.1
            );
        }
    }

    #[test]
    fn test_trait() {
        assert_eq!(
            PathBuf::from("/test/../path/").clean(),
            PathBuf::from("/path")
        );
    }
}
