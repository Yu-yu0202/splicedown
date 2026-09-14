mod utils;

use lib_2024_a::a_report;
use lib_2024_a::nested::deep::deep_mul;
use lib_2024_b::{b_calc, b_macro, b_macro_arg, b_use_internal};
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
    let c = lib_2024_b::c_const();
    println!("{s} {p} {b} {d} {m} {m2} {m3} {m4} {c}");
}
