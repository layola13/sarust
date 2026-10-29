pub fn triple(n: i32) -> i32 {
    n * 3
}

pub fn helper(n: i32) -> i32 {
    n + 100
}

#[cfg(feature = "fast")]
pub fn fast_mode(n: i32) -> i32 {
    n + 1000
}
