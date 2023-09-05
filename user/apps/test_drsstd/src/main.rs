#![no_std]
#![no_main]

extern crate drstd;

use drstd::print;
use drstd::println;

#[no_mangle]
fn main() {
    println!("Hello, world!");
}