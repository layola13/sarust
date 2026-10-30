fn probe() -> u64 {
    let a = 10i32;
    let b = 20i32;
    let c = 30i32;
    let d = 40i32;
    let v = vec![a, b, c, d];
    (v[0] + v[1] + v[2] + v[3]) as u64
}
fn main() { std::process::exit(probe() as i32); }
