pub mod deep;

pub const A_NESTED: i32 = 7;

pub fn nested_sum(xs: &[i64]) -> i64 {
    crate::twice(xs.iter().sum::<i64>()) + crate::A_CONST
}
