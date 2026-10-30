struct M { fd: i32, ptr: *const u8, len: i32 }
fn probe() -> u64 {
    let mut x = 7u8;
    let mut y = 9u8;
    let p = &x as *const u8;
    let q = &y as *const u8;
    let mut m = M { fd: 0, ptr: q, len: 4 };
    m.fd = 3;
    m.ptr = p;
    let eq = (m.ptr == p) as u64;
    eq * 100 + m.fd as u64 + m.len as u64
}
fn main() { std::process::exit(probe() as i32); }
