use intertrait::cast::*;
use intertrait::*;

#[cast_to(Greet)]
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

impl Source for Data {}

#[test]
fn test_cast_to_on_struct() {
    let data = Data;
    let source: &dyn Source = &data;
    let greet = source.cast::<dyn Greet>();
    greet.unwrap().greet();
}
