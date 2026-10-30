struct Point {
    x: i32,
    y: i32,
}
struct Pair(i32, i32);
struct Unit;
fn main() {
    let p = Point { x: 1, y: 2 };
    let q = Point { x: 5, ..p };
    let pr = Pair(3, 4);
    let _u = Unit;
    let sum = p.x + p.y + q.x + pr.0 + pr.1;
    let _ = sum;
}
