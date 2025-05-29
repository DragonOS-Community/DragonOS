use kdepends::another_ext4::Ext4Error;

impl From<Ext4Error> for super::SystemError {
    fn from(err: Ext4Error) -> Self {
        <Self as num_traits::FromPrimitive>::from_i32(err.code() as i32).unwrap()
    }
}
