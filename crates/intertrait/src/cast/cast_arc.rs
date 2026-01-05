use alloc::sync::Arc;

use crate::{caster, CastFromSync};

/// A trait that is blanket-implemented for traits extending `CastFrom` to allow for casting
/// of a trait object for it behind an `Rc` to a trait object for another trait
/// implemented by the underlying value.
///
/// # Examples
/// ```
/// # use std::sync::Arc;
/// # use intertrait::*;
/// use intertrait::cast::*;
///
/// # #[cast_to([sync] Greet)]
/// # struct Data;
/// # trait Source: CastFrom {}
/// # trait Greet {
/// #     fn greet(&self);
/// # }
/// # impl Greet for Data {
/// #    fn greet(&self) {
/// #        println!("Hello");
/// #    }
/// # }
/// impl Source for Data {}
/// let data = Data;
/// let source = Arc::new(data);
/// let greet = source.cast::<dyn Greet>();
/// greet.unwrap_or_else(|_| panic!("must not happen")).greet();
/// ```
pub trait CastArc {
    /// Casts an `Arc` for this trait into that for type `T`.
    fn cast<T: ?Sized + 'static>(self: Arc<Self>) -> Result<Arc<T>, Arc<Self>>;
}

/// A blanket implementation of `CastArc` for traits extending `CastFrom`, `Sync`, and `Send`.
impl<S: ?Sized + CastFromSync> CastArc for S {
    fn cast<T: ?Sized + 'static>(self: Arc<Self>) -> Result<Arc<T>, Arc<Self>> {
        match caster::<T>((*self).type_id()) {
            Some(caster) => Ok((caster.cast_arc)(self.arc_any())),
            None => Err(self),
        }
    }
}
