use std::fmt::Debug;

use intertrait::cast::*;
use intertrait::*;

struct Data;

trait Source: CastFrom {}

trait Concat<T: Debug> {
    type I1: Debug;
    type I2: Debug;

    fn concat(&self, prefix: T, a: Self::I1, b: Self::I2) -> String;
}

#[cast_to]
impl Concat<String> for Data {
    type I1 = i32;
    type I2 = &'static str;

    fn concat(&self, prefix: String, a: Self::I1, b: Self::I2) -> String {
        format!("{}: {} - {}", prefix, a, b)
    }
}

impl Source for Data {}

#[test]
fn test_cast_to_on_trait_impl_with_assoc_type3() {
    let data = Data;
    let source: &dyn Source = &data;
    let concat = source.cast::<dyn Concat<String, I1 = i32, I2 = &'static str>>();
    assert_eq!(
        concat.unwrap().concat("Data".to_owned(), 101, "hello"),
        "Data: 101 - hello"
    );
}
