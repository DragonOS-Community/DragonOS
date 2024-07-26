use std::fmt::Debug;

use intertrait::cast::*;
use intertrait::*;

struct I32Data(i32);

trait Source: CastFrom {}

trait Producer {
    type Output: Debug;

    fn produce(&self) -> Self::Output;
}

#[cast_to]
impl Producer for I32Data {
    type Output = i32;

    fn produce(&self) -> Self::Output {
        self.0
    }
}

impl Source for I32Data {}

#[test]
fn test_cast_to_on_trait_impl_with_assoc_type1() {
    let data = I32Data(100);
    let source: &dyn Source = &data;
    let producer = source.cast::<dyn Producer<Output = i32>>();
    assert_eq!(producer.unwrap().produce(), data.0);
}
