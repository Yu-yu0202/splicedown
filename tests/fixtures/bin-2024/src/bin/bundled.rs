#![allow(dead_code, unused_imports, unused_macros, unused_variables)]
mod __splicedown_lib_2024_a_0_0_0_5897ef93 {
    pub mod nested {
        pub mod deep {
            pub fn deep_mul(a: i64, b: i64) -> i128 {
                (super::A_NESTED as i128) * (a as i128) * (b as i128)
                    + crate::__splicedown_lib_2024_a_0_0_0_5897ef93::A_CONST as i128
            }
        }
        pub const A_NESTED: i32 = 7;
        pub fn nested_sum(xs: &[i64]) -> i64 {
            crate::__splicedown_lib_2024_a_0_0_0_5897ef93::twice(xs.iter().sum::<i64>())
                + crate::__splicedown_lib_2024_a_0_0_0_5897ef93::A_CONST
        }
    }
    pub const A_CONST: i64 = 100;
    pub fn twice(x: i64) -> i64 {
        x * 2
    }
    #[macro_export]
    macro_rules! a_report {
        ($v:expr) => {
            { let v = $v as i64; eprintln!("[a_report] {} = {}", stringify!($v),
            $crate::twice(v)); $crate::twice(v) }
        };
    }
}
mod __splicedown_lib_2024_b_0_0_0_32ebd302 {
    macro_rules! b_internal {
        ($v:expr) => {
            crate ::b_calc($v, 1) + lib_2024_a::twice($v)
        };
    }
    pub fn b_calc(a: i64, b: i64) -> i64 {
        crate::__splicedown_lib_2024_a_0_0_0_5897ef93::twice(a)
            + crate::__splicedown_lib_2024_a_0_0_0_5897ef93::nested::nested_sum(&[a, b])
            + crate::__splicedown_lib_2024_a_0_0_0_5897ef93::A_CONST
    }
    pub fn b_macro(v: i64) -> i64 {
        crate::__splicedown_lib_2024_a_0_0_0_5897ef93::a_report!(v)
    }
    pub fn b_macro_arg(v: i64) -> i64 {
        crate::__splicedown_lib_2024_a_0_0_0_5897ef93::a_report!(crate ::b_calc(v, 2))
    }
    pub fn b_use_internal(v: i64) -> i64 {
        b_internal!(v)
    }
    pub fn c_const() -> i64 {
        crate::__splicedown_lib_2024_c_0_0_0_64a847a5::C_CONST
    }
}
mod __splicedown_lib_2024_c_0_0_0_64a847a5 {
    pub const C_CONST: i64 = 42;
}
mod utils {
    pub(crate) fn add(a: i32, b: i32) -> i64 {
        a as i64 + b as i64
    }
    pub(crate) fn mul(a: i32, b: i32) -> i128 {
        a as i128 * b as i128
    }
}
use crate::__splicedown_lib_2024_a_0_0_0_5897ef93::a_report;
use crate::__splicedown_lib_2024_a_0_0_0_5897ef93::nested::deep::deep_mul;
use crate::__splicedown_lib_2024_b_0_0_0_32ebd302::{
    b_calc, b_macro, b_macro_arg, b_use_internal,
};
use lib_pm::PmDummy;
use utils::mul;
#[allow(unused)]
#[derive(PmDummy)]
struct Dummy;
fn main() {
    let s = utils::add(1, 2);
    let s2 = crate::utils::add(3, 4);
    let p = mul(2, 3);
    let b = b_calc(10, 20);
    let d = deep_mul(3, 4);
    let m = a_report!(s + s2);
    let m2 = b_macro(d as i64);
    let m3 = b_macro_arg(b);
    let m4 = b_use_internal(b);
    let c = crate::__splicedown_lib_2024_b_0_0_0_32ebd302::c_const();
    println!("{s} {p} {b} {d} {m} {m2} {m3} {m4} {c}");
}
