macro_rules! b_internal {
    ($v:expr) => {
        crate::b_calc($v, 1) + lib_2024_a::twice($v)
    };
}

pub fn b_calc(a: i64, b: i64) -> i64 {
    lib_2024_a::twice(a) + lib_2024_a::nested::nested_sum(&[a, b]) + lib_2024_a::A_CONST
}

pub fn b_macro(v: i64) -> i64 {
    lib_2024_a::a_report!(v)
}

pub fn b_macro_arg(v: i64) -> i64 {
    lib_2024_a::a_report!(crate::b_calc(v, 2))
}

pub fn b_use_internal(v: i64) -> i64 {
    b_internal!(v)
}
