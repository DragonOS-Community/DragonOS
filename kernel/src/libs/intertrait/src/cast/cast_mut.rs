use crate::{caster, CastFrom};

/// A trait that is blanket-implemented for traits extending `CastFrom` to allow for casting
/// of a trait object for it behind an mutable reference to a trait object for another trait
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
/// let mut data = Data;
/// let source: &mut dyn Source = &mut data;
/// let greet = source.cast::<dyn Greet>();
/// greet.unwrap().greet();
/// ```
pub trait CastMut {
    /// Casts a mutable reference to this trait into that of type `T`.
    fn cast<T: ?Sized + 'static>(&mut self) -> Option<&mut T>;
}

/// A blanket implementation of `CastMut` for traits extending `CastFrom`.
impl<S: ?Sized + CastFrom> CastMut for S {
    fn cast<T: ?Sized + 'static>(&mut self) -> Option<&mut T> {
        let any = self.mut_any();
        let caster = caster::<T>((*any).type_id())?;
        (caster.cast_mut)(any).into()
    }
}
