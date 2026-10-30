fn probe() -> u64 {
    let v = vec![10i32, 20, 30, 40];
    (v[0] + v[1] + v[2] + v[3]) as u64
}
fn main() { std::process::exit(probe() as i32); }
