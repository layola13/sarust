static mut SEED: i32 = 7;
fn probe() -> u64 {
    unsafe {
        SEED += 3;
        SEED as u64
    }
}
fn main() { std::process::exit(probe() as i32); }
