use intertrait::cast::*;
use intertrait::*;
use std::sync::Arc;

#[cast_to([sync, sync] Greet)]
struct Data;

trait Source: CastFromSync {}

trait Greet {
    fn greet(&self);
}

impl Greet for Data {
    fn greet(&self) {
        println!("Hello");
    }
}

impl Source for Data {}

fn main() {
    let data = Arc::new(Data);
    let source: Arc<dyn Source> = data;
    let greet = source.cast::<dyn Greet>();
    greet.unwrap_or_else(|_| panic!("can't happen")).greet();
}
