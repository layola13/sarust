enum Color {
    Red,
    Green,
    Blue,
}
enum Shape {
    Circle(f64),
    Rect { w: f64, h: f64 },
}
fn maybe_double(x: Option<i32>) -> Option<i32> {
    Some(x? * 2)
}
fn div(a: i32, b: i32) -> Result<i32, String> {
    if b == 0 {
        Err("zero".to_string())
    } else {
        Ok(a / b)
    }
}
fn main() {
    let c = Color::Red;
    let s = Shape::Circle(1.0);
    let m = maybe_double(Some(21));
    let d = div(10, 2);
    let _ = (c, s, m, d);
}
