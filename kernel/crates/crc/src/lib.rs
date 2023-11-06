#![cfg_attr(not(test), no_std)]

#[cfg(test)]
extern crate std;

pub mod crc64;
pub mod tables;

pub fn add(left: usize, right: usize) -> usize {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
