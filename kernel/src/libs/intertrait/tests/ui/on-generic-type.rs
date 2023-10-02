use intertrait::*;
use intertrait::cast::*;
use std::marker::PhantomData;

#[cast_to(Greet)]
struct Data<T: 'static> {
    phantom: PhantomData<T>,
}

trait Source: CastFrom {}

trait Greet {
    fn greet(&self);
}

impl<T: 'static> Greet for Data<T> {
    fn greet(&self) {
        println!("Hello");
    }
}

impl<T: 'static> Source for Data<T> {}

fn main() {
    let data = Data::<i32> {
        phantom: PhantomData,
    };
    let source: &dyn Source = &data;
    let greet = source.cast::<dyn Greet>();
    greet.unwrap().greet();
}
