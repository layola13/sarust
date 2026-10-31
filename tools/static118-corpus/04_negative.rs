static mut NEG: i32 = -3;
fn probe() -> u64 {
    unsafe {
        NEG += 5;
        NEG as u64
    }
}
fn main() { std::process::exit(probe() as i32); }
