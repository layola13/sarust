fn probe() -> u64 { let a = [11u64, 22, 33]; let r = &a[1]; *r }
fn main() { std::process::exit(probe() as i32); }
