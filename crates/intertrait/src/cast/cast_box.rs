use alloc::boxed::Box;

use crate::{caster, CastFrom};

/// A trait that is blanket-implemented for traits extending `CastFrom` to allow for casting
/// of a trait object for it behind a `Box` to a trait object for another trait
/// implemented by the underlying value.
///
/// # Examples
/// ```
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
/// let data = Box::new(Data);
/// let source: Box<dyn Source> = data;
/// let greet = source.cast::<dyn Greet>();
/// greet.unwrap_or_else(|_| panic!("casting failed")).greet();
/// ```
pub trait CastBox {
    /// Casts a box to this trait into that of type `T`. If fails, returns the receiver.
    fn cast<T: ?Sized + 'static>(self: Box<Self>) -> Result<Box<T>, Box<Self>>;
}

/// A blanket implementation of `CastBox` for traits extending `CastFrom`.
impl<S: ?Sized + CastFrom> CastBox for S {
    fn cast<T: ?Sized + 'static>(self: Box<Self>) -> Result<Box<T>, Box<Self>> {
        match caster::<T>((*self).type_id()) {
            Some(caster) => Ok((caster.cast_box)(self.box_any())),
            None => Err(self),
        }
    }
}
