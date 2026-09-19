mod utils;

use ::phase3_b::c_value;
use phase3_a::{self, nested::deep::deep_value, root_value};
use phase3_b as b;
use utils::mul;

fn main() {
    let local = utils::add(1, 2) + crate::utils::add(3, 4) + mul(2, 3);
    let deps = phase3_a::root_value() + root_value() + deep_value() + b::combined() + c_value();
    println!("{}", local + deps);
}
