//! `cast` module contains traits to provide `cast` method for various references
//! and smart pointers.
//!
//! In source files requiring casts, import all of the traits as follows:
//!
//! ```ignore
//! use intertrait::cast::*;
//! ```
//!
//! Since there exists single trait for each receiver type, the same `cast` method is overloaded.
mod cast_arc;
mod cast_box;
mod cast_mut;
mod cast_rc;
mod cast_ref;

pub use cast_arc::*;
pub use cast_box::*;
pub use cast_mut::*;
pub use cast_rc::*;
pub use cast_ref::*;
