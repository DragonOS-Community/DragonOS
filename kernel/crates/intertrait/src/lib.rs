//! A library providing direct casting among trait objects implemented by a type.
//!
//! In Rust, an object of a sub-trait of [`Any`] can be downcast to a concrete type
//! at runtime if the type is known. But no direct casting between two trait objects
//! (i.e. without involving the concrete type of the backing value) is possible
//! (even no coercion from a trait object to that of its super-trait yet).
//!
//! With this crate, any trait object with [`CastFrom`] as its super-trait can be cast directly
//! to another trait object implemented by the underlying type if the target traits are
//! registered beforehand with the macros provided by this crate.
//!
//! # Usage
//! ```
//! use intertrait::*;
//! use intertrait::cast::*;
//!
//! struct Data;
//!
//! trait Source: CastFrom {}
//!
//! trait Greet {
//!     fn greet(&self);
//! }
//!
//! #[cast_to]
//! impl Greet for Data {
//!     fn greet(&self) {
//!         println!("Hello");
//!     }
//! }
//!
//! impl Source for Data {}
//!
//! let data = Data;
//! let source: &dyn Source = &data;
//! let greet = source.cast::<dyn Greet>();
//! greet.unwrap().greet();
//! ```
//!
//! Target traits must be explicitly designated beforehand. There are three ways to do it:
//!
//! * [`#[cast_to]`][cast_to] to `impl` item
//! * [`#[cast_to(Trait)]`][cast_to] to type definition
//! * [`castable_to!(Type => Trait1, Trait2)`][castable_to]
//!
//! If the underlying type involved is `Sync + Send` and you want to use it with [`Arc`],
//! use [`CastFromSync`] in place of [`CastFrom`] and add `[sync]` flag before the list
//! of traits in the macros. Refer to the documents for each of macros for details.
//!
//! For casting, refer to traits defined in [`cast`] module.
//!
//! [cast_to]: ./attr.cast_to.html
//! [castable_to]: ./macro.castable_to.html
//! [`CastFrom`]: ./trait.CastFrom.html
//! [`CastFromSync`]: ./trait.CastFromSync.html
//! [`cast`]: ./cast/index.html
//! [`Any`]: https://doc.rust-lang.org/std/any/trait.Any.html
//! [`Arc`]: https://doc.rust-lang.org/std/sync/struct.Arc.html

#![cfg_attr(target_os = "none", no_std)]

extern crate alloc;
extern crate core;

use core::{
    any::{Any, TypeId},
    marker::{Send, Sync},
};

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::sync::Arc;

use hashbrown::HashMap;
use linkme::distributed_slice;

pub use intertrait_macros::*;

use crate::hasher::BuildFastHasher;

pub mod cast;
mod hasher;

#[doc(hidden)]
pub type BoxedCaster = Box<dyn Any + Send + Sync>;

#[cfg(doctest)]
doc_comment::doctest!("../README.md");

/// A distributed slice gathering constructor functions for [`Caster<T>`]s.
///
/// A constructor function returns `TypeId` of a concrete type involved in the casting
/// and a `Box` of a trait object backed by a [`Caster<T>`].
///
/// [`Caster<T>`]: ./struct.Caster.html
#[doc(hidden)]
#[distributed_slice]
pub static CASTERS: [fn() -> (TypeId, BoxedCaster)] = [..];

/// A `HashMap` mapping `TypeId` of a [`Caster<T>`] to an instance of it.
///
/// [`Caster<T>`]: ./struct.Caster.html
#[cfg(not(target_os = "none"))]
static CASTER_MAP: once_cell::sync::Lazy<HashMap<(TypeId, TypeId), BoxedCaster, BuildFastHasher>> =
    once_cell::sync::Lazy::new(|| {
        CASTERS
            .iter()
            .map(|f| {
                let (type_id, caster) = f();
                ((type_id, (*caster).type_id()), caster)
            })
            .collect()
    });

/// CasterMap
///
/// key.0: type_id of source
/// key.1: type_id of target
///
/// value: A BoxedCaster which can cast source to target
#[cfg(target_os = "none")]
static mut CASTER_MAP: Option<HashMap<(TypeId, TypeId), BoxedCaster, BuildFastHasher>> = None;

#[cfg(target_os = "none")]
#[allow(static_mut_refs)]
pub fn caster_map() -> &'static HashMap<(TypeId, TypeId), BoxedCaster, BuildFastHasher> {
    unsafe {
        CASTER_MAP.as_ref().unwrap_or_else(|| {
            panic!("intertrait_caster_map() must be called after CASTER_MAP is initialized")
        })
    }
}

/// Initializes the global [`CASTER_MAP`] with [`CASTERS`].
///
/// no_std环境下，需要手动调用此函数初始化CASTER_MAP
#[cfg(target_os = "none")]
pub fn init_caster_map() {
    use core::sync::atomic::AtomicBool;

    let pd = AtomicBool::new(false);
    let r = pd.compare_exchange(
        false,
        true,
        core::sync::atomic::Ordering::SeqCst,
        core::sync::atomic::Ordering::SeqCst,
    );

    if r.is_err() {
        panic!("init_caster_map() must be called only once");
    }

    let hashmap = CASTERS
        .iter()
        .map(|f| {
            let (type_id, caster) = f();
            ((type_id, (*caster).type_id()), caster)
        })
        .collect();

    unsafe { CASTER_MAP = Some(hashmap) };
}

#[cfg(not(target_os = "none"))]
pub fn init_caster_map() {}

fn cast_arc_panic<T: ?Sized + 'static>(_: Arc<dyn Any + Sync + Send>) -> Arc<T> {
    panic!("Prepend [sync] to the list of target traits for Sync + Send types")
}

/// A `Caster` knows how to cast a reference to or `Box` of a trait object for `Any`
/// to a trait object of trait `T`. Each `Caster` instance is specific to a concrete type.
/// That is, it knows how to cast to single specific trait implemented by single specific type.
///
/// An implementation of a trait for a concrete type doesn't need to manually provide
/// a `Caster`. Instead attach `#[cast_to]` to the `impl` block.
#[doc(hidden)]
pub struct Caster<T: ?Sized + 'static> {
    /// Casts an immutable reference to a trait object for `Any` to a reference
    /// to a trait object for trait `T`.
    pub cast_ref: fn(from: &dyn Any) -> &T,

    /// Casts a mutable reference to a trait object for `Any` to a mutable reference
    /// to a trait object for trait `T`.
    pub cast_mut: fn(from: &mut dyn Any) -> &mut T,

    /// Casts a `Box` holding a trait object for `Any` to another `Box` holding a trait object
    /// for trait `T`.
    pub cast_box: fn(from: Box<dyn Any>) -> Box<T>,

    /// Casts an `Rc` holding a trait object for `Any` to another `Rc` holding a trait object
    /// for trait `T`.
    pub cast_rc: fn(from: Rc<dyn Any>) -> Rc<T>,

    /// Casts an `Arc` holding a trait object for `Any + Sync + Send + 'static`
    /// to another `Arc` holding a trait object for trait `T`.
    pub cast_arc: fn(from: Arc<dyn Any + Sync + Send + 'static>) -> Arc<T>,
}

impl<T: ?Sized + 'static> Caster<T> {
    pub fn new(
        cast_ref: fn(from: &dyn Any) -> &T,
        cast_mut: fn(from: &mut dyn Any) -> &mut T,
        cast_box: fn(from: Box<dyn Any>) -> Box<T>,
        cast_rc: fn(from: Rc<dyn Any>) -> Rc<T>,
    ) -> Caster<T> {
        Caster::<T> {
            cast_ref,
            cast_mut,
            cast_box,
            cast_rc,
            cast_arc: cast_arc_panic,
        }
    }

    pub fn new_sync(
        cast_ref: fn(from: &dyn Any) -> &T,
        cast_mut: fn(from: &mut dyn Any) -> &mut T,
        cast_box: fn(from: Box<dyn Any>) -> Box<T>,
        cast_rc: fn(from: Rc<dyn Any>) -> Rc<T>,
        cast_arc: fn(from: Arc<dyn Any + Sync + Send>) -> Arc<T>,
    ) -> Caster<T> {
        Caster::<T> {
            cast_ref,
            cast_mut,
            cast_box,
            cast_rc,
            cast_arc,
        }
    }
}

/// Returns a `Caster<S, T>` from a concrete type `S` to a trait `T` implemented by it.
///
/// ## 参数
///
/// - type_id: 源类型的type_id
///
/// T: 目标trait
fn caster<T: ?Sized + 'static>(type_id: TypeId) -> Option<&'static Caster<T>> {
    #[cfg(not(target_os = "none"))]
    {
        CASTER_MAP
            .get(&(type_id, TypeId::of::<Caster<T>>()))
            .and_then(|caster| caster.downcast_ref::<Caster<T>>())
    }

    #[cfg(target_os = "none")]
    {
        caster_map()
            .get(&(type_id, TypeId::of::<Caster<T>>()))
            .and_then(|caster| caster.downcast_ref::<Caster<T>>())
    }
}

/// `CastFrom` must be extended by a trait that wants to allow for casting into another trait.
///
/// It is used for obtaining a trait object for [`Any`] from a trait object for its sub-trait,
/// and blanket implemented for all `Sized + Any + 'static` types.
///
/// # Examples
/// ```ignore
/// trait Source: CastFrom {
///     ...
/// }
/// ```
pub trait CastFrom: Any + 'static {
    /// Returns a immutable reference to `Any`, which is backed by the type implementing this trait.
    fn ref_any(&self) -> &dyn Any;

    /// Returns a mutable reference to `Any`, which is backed by the type implementing this trait.
    fn mut_any(&mut self) -> &mut dyn Any;

    /// Returns a `Box` of `Any`, which is backed by the type implementing this trait.
    fn box_any(self: Box<Self>) -> Box<dyn Any>;

    /// Returns an `Rc` of `Any`, which is backed by the type implementing this trait.
    fn rc_any(self: Rc<Self>) -> Rc<dyn Any>;
}

/// `CastFromSync` must be extended by a trait that is `Any + Sync + Send + 'static`
/// and wants to allow for casting into another trait behind references and smart pointers
/// especially including `Arc`.
///
/// It is used for obtaining a trait object for [`Any + Sync + Send + 'static`] from an object
/// for its sub-trait, and blanket implemented for all `Sized + Sync + Send + 'static` types.
///
/// # Examples
/// ```ignore
/// trait Source: CastFromSync {
///     ...
/// }
/// ```
pub trait CastFromSync: CastFrom + Sync + Send + 'static {
    fn arc_any(self: Arc<Self>) -> Arc<dyn Any + Sync + Send + 'static>;
}

impl<T: Sized + Any + 'static> CastFrom for T {
    fn ref_any(&self) -> &dyn Any {
        self
    }

    fn mut_any(&mut self) -> &mut dyn Any {
        self
    }

    fn box_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }

    fn rc_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

impl CastFrom for dyn Any + 'static {
    fn ref_any(&self) -> &dyn Any {
        self
    }

    fn mut_any(&mut self) -> &mut dyn Any {
        self
    }

    fn box_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }

    fn rc_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

impl<T: Sized + Sync + Send + 'static> CastFromSync for T {
    fn arc_any(self: Arc<Self>) -> Arc<dyn Any + Sync + Send + 'static> {
        self
    }
}

impl CastFrom for dyn Any + Sync + Send + 'static {
    fn ref_any(&self) -> &dyn Any {
        self
    }

    fn mut_any(&mut self) -> &mut dyn Any {
        self
    }

    fn box_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }

    fn rc_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

impl CastFromSync for dyn Any + Sync + Send + 'static {
    fn arc_any(self: Arc<Self>) -> Arc<dyn Any + Sync + Send + 'static> {
        self
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::any::{Any, TypeId};
    use std::fmt::{Debug, Display};

    use linkme::distributed_slice;

    use crate::{BoxedCaster, CastFromSync};

    use super::cast::*;
    use super::*;

    #[distributed_slice(super::CASTERS)]
    static TEST_CASTER: fn() -> (TypeId, BoxedCaster) = create_test_caster;

    #[derive(Debug)]
    struct TestStruct;

    trait SourceTrait: CastFromSync {}

    impl SourceTrait for TestStruct {}

    fn create_test_caster() -> (TypeId, BoxedCaster) {
        let type_id = TypeId::of::<TestStruct>();
        let caster = Box::new(Caster::<dyn Debug> {
            cast_ref: |from| from.downcast_ref::<TestStruct>().unwrap(),
            cast_mut: |from| from.downcast_mut::<TestStruct>().unwrap(),
            cast_box: |from| from.downcast::<TestStruct>().unwrap(),
            cast_rc: |from| from.downcast::<TestStruct>().unwrap(),
            cast_arc: |from| from.downcast::<TestStruct>().unwrap(),
        });
        (type_id, caster)
    }

    #[test]
    fn cast_ref() {
        let ts = TestStruct;
        let st: &dyn SourceTrait = &ts;
        let debug = st.cast::<dyn Debug>();
        assert!(debug.is_some());
    }

    #[test]
    fn cast_mut() {
        let mut ts = TestStruct;
        let st: &mut dyn SourceTrait = &mut ts;
        let debug = st.cast::<dyn Debug>();
        assert!(debug.is_some());
    }

    #[test]
    fn cast_box() {
        let ts = Box::new(TestStruct);
        let st: Box<dyn SourceTrait> = ts;
        let debug = st.cast::<dyn Debug>();
        assert!(debug.is_ok());
    }

    #[test]
    fn cast_rc() {
        let ts = Rc::new(TestStruct);
        let st: Rc<dyn SourceTrait> = ts;
        let debug = st.cast::<dyn Debug>();
        assert!(debug.is_ok());
    }

    #[test]
    fn cast_arc() {
        let ts = Arc::new(TestStruct);
        let st: Arc<dyn SourceTrait> = ts;
        let debug = st.cast::<dyn Debug>();
        assert!(debug.is_ok());
    }

    #[test]
    fn cast_ref_wrong() {
        let ts = TestStruct;
        let st: &dyn SourceTrait = &ts;
        let display = st.cast::<dyn Display>();
        assert!(display.is_none());
    }

    #[test]
    fn cast_mut_wrong() {
        let mut ts = TestStruct;
        let st: &mut dyn SourceTrait = &mut ts;
        let display = st.cast::<dyn Display>();
        assert!(display.is_none());
    }

    #[test]
    fn cast_box_wrong() {
        let ts = Box::new(TestStruct);
        let st: Box<dyn SourceTrait> = ts;
        let display = st.cast::<dyn Display>();
        assert!(display.is_err());
    }

    #[test]
    fn cast_rc_wrong() {
        let ts = Rc::new(TestStruct);
        let st: Rc<dyn SourceTrait> = ts;
        let display = st.cast::<dyn Display>();
        assert!(display.is_err());
    }

    #[test]
    fn cast_arc_wrong() {
        let ts = Arc::new(TestStruct);
        let st: Arc<dyn SourceTrait> = ts;
        let display = st.cast::<dyn Display>();
        assert!(display.is_err());
    }

    #[test]
    fn cast_ref_from_any() {
        let ts = TestStruct;
        let st: &dyn Any = &ts;
        let debug = st.cast::<dyn Debug>();
        assert!(debug.is_some());
    }

    #[test]
    fn cast_mut_from_any() {
        let mut ts = TestStruct;
        let st: &mut dyn Any = &mut ts;
        let debug = st.cast::<dyn Debug>();
        assert!(debug.is_some());
    }

    #[test]
    fn cast_box_from_any() {
        let ts = Box::new(TestStruct);
        let st: Box<dyn Any> = ts;
        let debug = st.cast::<dyn Debug>();
        assert!(debug.is_ok());
    }

    #[test]
    fn cast_rc_from_any() {
        let ts = Rc::new(TestStruct);
        let st: Rc<dyn Any> = ts;
        let debug = st.cast::<dyn Debug>();
        assert!(debug.is_ok());
    }

    #[test]
    fn cast_arc_from_any() {
        let ts = Arc::new(TestStruct);
        let st: Arc<dyn Any + Send + Sync> = ts;
        let debug = st.cast::<dyn Debug>();
        assert!(debug.is_ok());
    }

    #[test]
    fn impls_ref() {
        let ts = TestStruct;
        let st: &dyn SourceTrait = &ts;
        assert!(st.impls::<dyn Debug>());
    }

    #[test]
    fn impls_mut() {
        let mut ts = TestStruct;
        let st: &mut dyn SourceTrait = &mut ts;
        assert!((*st).impls::<dyn Debug>());
    }

    #[test]
    fn impls_box() {
        let ts = Box::new(TestStruct);
        let st: Box<dyn SourceTrait> = ts;
        assert!((*st).impls::<dyn Debug>());
    }

    #[test]
    fn impls_rc() {
        let ts = Rc::new(TestStruct);
        let st: Rc<dyn SourceTrait> = ts;
        assert!((*st).impls::<dyn Debug>());
    }

    #[test]
    fn impls_arc() {
        let ts = Arc::new(TestStruct);
        let st: Arc<dyn SourceTrait> = ts;
        assert!((*st).impls::<dyn Debug>());
    }

    #[test]
    fn impls_not_ref() {
        let ts = TestStruct;
        let st: &dyn SourceTrait = &ts;
        assert!(!st.impls::<dyn Display>());
    }

    #[test]
    fn impls_not_mut() {
        let mut ts = TestStruct;
        let st: &mut dyn Any = &mut ts;
        assert!(!(*st).impls::<dyn Display>());
    }

    #[test]
    fn impls_not_box() {
        let ts = Box::new(TestStruct);
        let st: Box<dyn SourceTrait> = ts;
        assert!(!st.impls::<dyn Display>());
    }

    #[test]
    fn impls_not_rc() {
        let ts = Rc::new(TestStruct);
        let st: Rc<dyn SourceTrait> = ts;
        assert!(!(*st).impls::<dyn Display>());
    }

    #[test]
    fn impls_not_arc() {
        let ts = Arc::new(TestStruct);
        let st: Arc<dyn SourceTrait> = ts;
        assert!(!(*st).impls::<dyn Display>());
    }
}
