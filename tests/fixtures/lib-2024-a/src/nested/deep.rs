pub fn deep_mul(a: i64, b: i64) -> i128 {
    (super::A_NESTED as i128) * (a as i128) * (b as i128) + crate::A_CONST as i128
}
