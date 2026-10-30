fn main() {
    let mut sum = 0;
    for i in 0..10 {
        if i == 5 {
            continue;
        }
        if i == 8 {
            break;
        }
        sum += i;
    }
    let mut j = 0;
    while j < 3 {
        j += 1;
    }
    let mut k = 0;
    loop {
        k += 1;
        if k >= 2 {
            break;
        }
    }
    let m = match sum {
        0 => "zero",
        1..=10 => "small",
        n if n > 20 => "big",
        _ => "other",
    };
    let _ = (j, k, m);
}
