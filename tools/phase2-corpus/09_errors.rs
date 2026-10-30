fn parse_or_zero(s: &str) -> i32 {
    s.parse::<i32>().unwrap_or(0)
}
fn must_get(v: Vec<i32>) -> i32 {
    v.into_iter().next().expect("empty")
}
fn sum_parsed(a: &str, b: &str) -> Result<i32, String> {
    let x: i32 = a.parse().map_err(|_| "bad a".to_string())?;
    let y: i32 = b.parse().map_err(|_| "bad b".to_string())?;
    Ok(x + y)
}
fn main() {
    let a = parse_or_zero("42");
    let b = must_get(vec![7]);
    let c = sum_parsed("3", "4").unwrap();
    let d = if a > 100 { panic!("too big") } else { a };
    let _ = (b, c, d);
}
