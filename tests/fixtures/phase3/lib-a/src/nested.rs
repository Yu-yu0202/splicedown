pub mod deep;

pub const NESTED: i32 = 2;

pub fn nested_value() -> i32 {
    crate::root_value() + crate::BASE
}
