const MAX: i32 = 100;
static GREETING: &str = "hello";
fn main() {
    let x = 5;
    let mut y = 10;
    y += 1;
    let x = x + 1;
    let z = MAX + y + x;
    let g = GREETING;
    let _ = (z, g);
}
