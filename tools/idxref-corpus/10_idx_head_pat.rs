fn probe() -> u64 { let a = [11u64, 22, 33]; let s: &[u64] = &a; match s { [x, ..] => *x, [] => 0 } }
fn main() { std::process::exit(probe() as i32); }
