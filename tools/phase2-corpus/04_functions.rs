fn fact(n: u32) -> u32 {
    if n <= 1 {
        1
    } else {
        n * fact(n - 1)
    }
}
struct Counter {
    n: i32,
}
impl Counter {
    fn new() -> Counter {
        Counter { n: 0 }
    }
    fn inc(&mut self) {
        self.n += 1;
    }
    fn get(&self) -> i32 {
        self.n
    }
}
fn main() {
    let f = fact(5);
    let mut c = Counter::new();
    c.inc();
    let g = c.get();
    let _ = (f, g);
}
