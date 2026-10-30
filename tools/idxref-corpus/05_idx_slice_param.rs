fn get(a: &[u64], i: usize) -> u64 { a[i] }
fn probe() -> u64 { let a = [11u64, 22, 33]; get(&a, 2) }
fn main() { std::process::exit(probe() as i32); }
