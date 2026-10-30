fn probe() -> u64 { let a = [11u64, 22, 33]; match &a { [x, .., y] => *x + *y, _ => 0 } }
fn main() { std::process::exit(probe() as i32); }
