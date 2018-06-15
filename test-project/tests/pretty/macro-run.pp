#![feature(prelude_import)]
#![no_std]
#[prelude_import]
use std::prelude::v1::*;
#[macro_use]
extern crate std;
macro_rules! square(( $ x : expr ) => { $ x * $ x } ;);

fn main() { 5 * 5; }
