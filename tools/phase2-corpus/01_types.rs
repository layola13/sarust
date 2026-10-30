fn main() {
    let a: i32 = 42;
    let b: f64 = 3.5;
    let c: bool = true;
    let d: char = 'x';
    let t: (i32, &str, bool) = (1, "hi", false);
    let arr: [i32; 4] = [1, 2, 3, 4];
    let sl: &[i32] = &arr[1..3];
    let s: String = String::from("hello");
    let st: &str = s.as_str();
    let f = b + 1.5;
    let sum = a + arr[0] + sl[0] + t.0;
    let _ = (c, d, st, f, sum);
}
