fn probe() -> u64 { let a = [11u64, 22, 33]; let i = 2usize; a[i] + a[i - 1] }
fn main() { std::process::exit(probe() as i32); }
