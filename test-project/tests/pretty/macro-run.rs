macro_rules! square {
    ($x:expr) => {
        $x * $x
    };
}

fn main() {
    square!(5);
}
