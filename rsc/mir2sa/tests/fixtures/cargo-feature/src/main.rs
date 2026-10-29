#[cfg(feature = "fast")]
fn mode() -> i32 {
    1
}

#[cfg(not(feature = "fast"))]
fn mode() -> i32 {
    0
}

fn main() {
    let v = mode();
    let _ = v;
}
