#![cfg_attr(not(test), no_std)]
#![feature(const_for)]
#![feature(const_mut_refs)]
#![feature(const_trait_impl)]
#![allow(clippy::needless_return)]

#[cfg(test)]
extern crate std;

pub mod crc64;
pub mod tables;
