pub mod nested;

pub const A_CONST: i64 = 100;

pub fn twice(x: i64) -> i64 {
    x * 2
}

#[macro_export]
macro_rules! a_report {
    ($v:expr) => {{
        let v = $v as i64;
        eprintln!("[a_report] {} = {}", stringify!($v), $crate::twice(v));
        $crate::twice(v)
    }};
}
