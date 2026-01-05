use crate::{caster, CastFrom};
use alloc::rc::Rc;

/// A trait that is blanket-implemented for traits extending `CastFrom` to allow for casting
/// of a trait object for it behind an `Rc` to a trait object for another trait
/// implemented by the underlying value.
///
/// # Examples
/// ```
/// # use std::rc::Rc;
/// # use intertrait::*;
/// use intertrait::cast::*;
///
/// # #[cast_to(Greet)]
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
/// let source = Rc::new(data);
/// let greet = source.cast::<dyn Greet>();
/// greet.unwrap_or_else(|_| panic!("must not happen")).greet();
/// ```
pub trait CastRc {
    /// Casts an `Rc` for this trait into that for type `T`.
    fn cast<T: ?Sized + 'static>(self: Rc<Self>) -> Result<Rc<T>, Rc<Self>>;
}

/// A blanket implementation of `CastRc` for traits extending `CastFrom`.
impl<S: ?Sized + CastFrom> CastRc for S {
    fn cast<T: ?Sized + 'static>(self: Rc<Self>) -> Result<Rc<T>, Rc<Self>> {
        match caster::<T>((*self).type_id()) {
            Some(caster) => Ok((caster.cast_rc)(self.rc_any())),
            None => Err(self),
        }
    }
}
