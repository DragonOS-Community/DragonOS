use intertrait::cast::*;
use intertrait::*;

#[cast_to(Greet, Greet1, Greet2)]
struct Data;

trait Source: CastFrom {}

trait Greet {
    fn greet(&self);
}

impl Greet for Data {
    fn greet(&self) {
        println!("Hello");
    }
}

trait Greet1 {
    fn greet1(&self);
}

impl Greet1 for Data {
    fn greet1(&self) {
        println!("Hello1");
    }
}

trait Greet2 {
    fn greet2(&self);
}

impl Greet2 for Data {
    fn greet2(&self) {
        println!("Hello2");
    }
}

impl Source for Data {}

#[test]
fn test_multi_traits_on_struct() {
    let data = Data;
    let source: &dyn Source = &data;

    let greet = source.cast::<dyn Greet>();
    greet.unwrap().greet();

    let greet1 = source.cast::<dyn Greet1>();
    greet1.unwrap().greet1();

    let greet2 = source.cast::<dyn Greet2>();
    greet2.unwrap().greet2();
}
