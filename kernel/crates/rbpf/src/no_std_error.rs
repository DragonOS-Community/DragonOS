// SPDX-License-Identifier: (Apache-2.0 OR MIT)

//! This module provides a simple implementation of the Error struct that is
//! used as a drop-in replacement for `std::io::Error` when using `rbpf` in `no_std`.

use crate::lib::String;

/// Implementation of Error for no_std applications.
/// Ensures that the existing code can use it with the same interface
/// as the Error from std::io::Error.
#[derive(Debug)]
pub struct Error {
    #[allow(dead_code)]
    kind: ErrorKind,
    #[allow(dead_code)]
    error: String,
}

impl Error {
    /// New function exposing the same signature as `std::io::Error::new`.
    #[allow(dead_code)]
    pub fn new<S: Into<String>>(kind: ErrorKind, error: S) -> Error {
        Error {
            kind,
            error: error.into(),
        }
    }
}

/// The current version of `rbpf` only uses the [`Other`](ErrorKind::Other) variant
/// from the [std::io::ErrorKind] enum. If a dependency on other variants were
/// introduced in the future, this enum needs to be updated accordingly to maintain
/// compatibility with the real `ErrorKind`. The reason all available variants
/// aren't included in the first place is that [std::io::ErrorKind] exposes
/// 40 variants, and not all of them are meaningful under `no_std`.
#[derive(Debug)]
pub enum ErrorKind {
    /// The no_std code only uses this variant.
    #[allow(dead_code)]
    Other,
}
