static mut COUNTER: i32 = 0;
fn probe() -> u64 {
    unsafe {
        COUNTER += 2;
        COUNTER += 3;
        COUNTER as u64
    }
}
fn main() { std::process::exit(probe() as i32); }
