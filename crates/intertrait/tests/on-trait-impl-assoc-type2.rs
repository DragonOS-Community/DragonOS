use std::fmt::Debug;

use intertrait::cast::*;
use intertrait::*;

struct Data;

trait Source: CastFrom {}

trait Concat {
    type I1: Debug;
    type I2: Debug;

    fn concat(&self, a: Self::I1, b: Self::I2) -> String;
}

#[cast_to]
impl Concat for Data {
    type I1 = i32;
    type I2 = &'static str;

    fn concat(&self, a: Self::I1, b: Self::I2) -> String {
        format!("Data: {} - {}", a, b)
    }
}

impl Source for Data {}

#[test]
fn test_cast_to_on_trait_impl_with_assoc_type2() {
    let data = Data;
    let source: &dyn Source = &data;
    let concat = source.cast::<dyn Concat<I1 = i32, I2 = &'static str>>();
    assert_eq!(concat.unwrap().concat(101, "hello"), "Data: 101 - hello");
}
