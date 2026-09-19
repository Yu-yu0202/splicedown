use crate as this_crate;

fn local_value() -> i32 {
    1
}

pub fn combined() -> i32 {
    this_crate::local_value()
        + phase3_a::root_value()
        + phase3_a::nested::nested_value()
        + phase3_c::VALUE
}

pub fn c_value() -> i32 {
    phase3_c::VALUE
}
