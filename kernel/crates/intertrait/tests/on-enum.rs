use intertrait::cast::*;
use intertrait::*;

#[cast_to(Greet)]
#[allow(dead_code)]
enum Data {
    Var1,
    Var2(u32),
}

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
fn test_cast_to_on_enum() {
    let data = Data::Var2(1);
    let source: &dyn Source = &data;
    let greet = source.cast::<dyn Greet>();
    greet.unwrap().greet();
}
