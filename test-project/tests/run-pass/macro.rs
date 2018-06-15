extern crate serde_derive;

macro_rules! square {
    ($x:expr) => {
        $x * $x
    };
}

fn f() -> i8 {
    square!(5)
}

fn main() {}
