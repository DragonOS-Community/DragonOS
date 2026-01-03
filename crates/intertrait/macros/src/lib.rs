extern crate proc_macro;

use proc_macro::TokenStream;

use syn::{parse, parse_macro_input, DeriveInput, ItemImpl};

use args::{Casts, Flag, Targets};
use gen_caster::generate_caster;

mod args;
mod gen_caster;
mod item_impl;
mod item_type;

/// Attached on an `impl` item or type definition, registers traits as targets for casting.
///
/// If on an `impl` item, no argument is allowed. But on a type definition, the target traits
/// must be listed explicitly.
///
/// Add `[sync]` before the list of traits if the underlying type is `Sync + Send` and you
/// need `std::sync::Arc`.
///
/// # Examples
/// ## On a trait impl
/// ```
/// use intertrait::*;
///
/// struct Data;
///
/// trait Greet {
///     fn greet(&self);
/// }
///
/// // Greet can be cast into from any sub-trait of CastFrom implemented by Data.
/// #[cast_to]
/// impl Greet for Data {
///     fn greet(&self) {
///         println!("Hello");
///     }
/// }
/// ```
///
/// ## On a type definition
/// Use when a target trait is derived or implemented in an external crate.
/// ```
/// use intertrait::*;
///
/// // Debug can be cast into from any sub-trait of CastFrom implemented by Data
/// #[cast_to(std::fmt::Debug)]
/// #[derive(std::fmt::Debug)]
/// struct Data;
/// ```
///
/// ## For Arc
/// Use when the underlying type is `Sync + Send` and you want to use `Arc`.
/// ```
/// use intertrait::*;
///
/// // Debug can be cast into from any sub-trait of CastFrom implemented by Data
/// #[cast_to([sync] std::fmt::Debug)]
/// #[derive(std::fmt::Debug)]
/// struct Data;
/// ```
#[proc_macro_attribute]
pub fn cast_to(args: TokenStream, input: TokenStream) -> TokenStream {
    match parse::<Targets>(args) {
        Ok(Targets { flags, paths }) => {
            if paths.is_empty() {
                item_impl::process(&flags, parse_macro_input!(input as ItemImpl))
            } else {
                item_type::process(&flags, paths, parse_macro_input!(input as DeriveInput))
            }
        }
        Err(err) => vec![err.to_compile_error(), input.into()]
            .into_iter()
            .collect(),
    }
    .into()
}

/// Declares target traits for casting implemented by a type.
///
/// This macro is for registering both a concrete type and its traits to be targets for casting.
/// Useful when the type definition and the trait implementations are in an external crate.
///
/// **Note**: this macro cannot be used in an expression or statement prior to Rust 1.45.0,
/// due to [a previous limitation](https://github.com/rust-lang/rust/pull/68717).
/// If you want to use it in an expression or statement, use Rust 1.45.0 or later.
///
/// # Examples
/// ```
/// use intertrait::*;
///
/// #[derive(std::fmt::Debug)]
/// enum Data {
///     A, B, C
/// }
/// trait Greet {
///     fn greet(&self);
/// }
/// impl Greet for Data {
///     fn greet(&self) {
///         println!("Hello");
///     }
/// }
///
/// castable_to! { Data => std::fmt::Debug, Greet }
///
/// # fn main() {}
/// ```
///
/// When the type is `Sync + Send` and is used with `Arc`:
/// ```
/// use intertrait::*;
///
/// #[derive(std::fmt::Debug)]
/// enum Data {
///     A, B, C
/// }
/// trait Greet {
///     fn greet(&self);
/// }
/// impl Greet for Data {
///     fn greet(&self) {
///         println!("Hello");
///     }
/// }
/// castable_to! { Data => [sync] std::fmt::Debug, Greet }
///
/// # fn main() {}
/// ```
#[proc_macro]
pub fn castable_to(input: TokenStream) -> TokenStream {
    let Casts {
        ty,
        targets: Targets { flags, paths },
    } = parse_macro_input!(input);

    paths
        .iter()
        .map(|t| generate_caster(&ty, t, flags.contains(&Flag::Sync)))
        .collect::<proc_macro2::TokenStream>()
        .into()
}
