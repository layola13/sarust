fn probe() -> u64 { let a = [11u8, 22, 33]; a[1] as u64 }
fn main() { std::process::exit(probe() as i32); }
