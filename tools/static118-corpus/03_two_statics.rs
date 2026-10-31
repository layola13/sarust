static mut A: i32 = 1;
static mut B: i32 = 2;
fn probe() -> u64 {
    unsafe {
        A += 10;
        B += 20;
        (A + B) as u64
    }
}
fn main() { std::process::exit(probe() as i32); }
