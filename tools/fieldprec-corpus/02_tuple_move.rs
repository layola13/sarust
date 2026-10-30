fn probe() -> u64 { let t = (11u64, 22, 33); let u = t; u.2 }
fn main() { std::process::exit(probe() as i32); }
