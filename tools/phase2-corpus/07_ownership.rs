fn first(s: &str) -> &str {
    s
}
fn append(v: &mut Vec<i32>, x: i32) {
    v.push(x);
}
fn main() {
    let s = String::from("abc");
    let flen = first(&s).len();
    let moved = s;
    let mut v: Vec<i32> = Vec::new();
    append(&mut v, 1);
    let cp = 5;
    let cp2 = cp;
    let _ = (flen, moved, v, cp, cp2);
}
