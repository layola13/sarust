fn probe() -> u64 { let a = [-5i32, 3]; if a[0] < a[1] { 7 } else { 8 } }
fn main() { std::process::exit(probe() as i32); }
