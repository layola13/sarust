fn probe() -> u64 { let a = [11u32, 22, 33]; a[2] as u64 }
fn main() { std::process::exit(probe() as i32); }
