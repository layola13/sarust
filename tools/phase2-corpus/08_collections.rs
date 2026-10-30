use std::collections::HashMap;
fn main() {
    let mut v: Vec<i32> = Vec::new();
    v.push(1);
    v.push(2);
    let doubled: Vec<i32> = v.iter().map(|x| x * 2).filter(|x| x % 4 == 0).collect();
    let mut m: HashMap<&str, i32> = HashMap::new();
    m.insert("a", 1);
    let got = m.get("a");
    let _ = (doubled, got);
}
