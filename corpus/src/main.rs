//! rsc full-support corpus: one fn per construct, borrowck-valid, std-only.
//! Each fn gets its own MIR body so `mir2sa coverage` pinpoints gaps per kind.

use std::collections::HashMap;
use std::rc::Rc;

// --- enums, match, guards, SetDiscriminant shapes ---
pub enum Shape {
    None,
    Point(i32, i32),
    Named { x: i32, y: i32 },
}

pub fn f_match(s: Shape) -> i32 {
    match s {
        Shape::None => 0,
        Shape::Point(x, y) => x + y,
        Shape::Named { x, y } if x > y => x - y,
        Shape::Named { x, y } => x + y,
    }
}

pub fn f_opt_mutate(o: &mut Option<i32>) -> i32 {
    *o = Some(41);
    *o = None;
    *o = Some(1);
    o.unwrap_or(0)
}

// --- structs, tuples, field/index projections, slices ---
pub struct Pt {
    pub x: i32,
    pub y: i32,
}

pub fn f_struct(mut p: Pt) -> i32 {
    p.x += 1;
    p.y *= 2;
    p.x - p.y
}

pub fn f_tuple(t: (i32, String, bool)) -> usize {
    t.1.len() + t.0 as usize + t.2 as usize
}

pub fn f_slice(a: &mut [i32]) -> i32 {
    let mut s = 0;
    let n = a.len();
    let mut i = 0;
    while i < n {
        a[i] += 1;
        s += a[i];
        i += 1;
    }
    s
}

pub fn f_repeat() -> [u8; 8] {
    [7u8; 8]
}

// --- loops, ranges, for/iter ---
pub fn f_loops(v: &[i32]) -> i32 {
    let mut s = 0;
    for x in v.iter() {
        s += *x;
    }
    let mut i = 0;
    loop {
        if i >= 3 {
            break;
        }
        s += i;
        i += 1;
    }
    'outer: for a in 0..4 {
        for b in 0..4 {
            if a + b > 4 {
                break 'outer;
            }
            s += 1;
        }
    }
    s
}

// --- closures, captures, moves ---
pub fn f_closure(v: Vec<i32>) -> Vec<i32> {
    let k = 10;
    let add = |x: i32| x + k;
    let mut w: Vec<i32> = v.into_iter().map(add).collect();
    let mut acc = 0;
    let mut bump = move |x: i32| {
        acc += x;
        acc
    };
    w.push(bump(1));
    w.push(bump(2));
    w
}

// --- generics, monomorphization, trait bounds ---
pub fn f_generic<T: Clone + Default>(x: T) -> (T, T) {
    (x.clone(), T::default())
}

pub trait Shout {
    fn shout(&self) -> String;
}

pub struct Cat;
pub struct Dog;

impl Shout for Cat {
    fn shout(&self) -> String {
        String::from("miao")
    }
}

impl Shout for Dog {
    fn shout(&self) -> String {
        String::from("wang")
    }
}

pub fn f_dyn(s: &dyn Shout) -> usize {
    s.shout().len()
}

pub fn f_static_dispatch<T: Shout>(s: T) -> usize {
    s.shout().len()
}

// --- Result/Option/? ---
pub fn f_parse(s: &str) -> Result<i32, String> {
    let a: i32 = s.parse().map_err(|_| String::from("bad"))?;
    Ok(a * 2)
}

// --- smart pointers, interior patterns ---
pub fn f_box(v: Vec<i32>) -> i32 {
    let b = Box::new(v);
    b.iter().sum()
}

pub fn f_rc(s: &str) -> usize {
    let a = Rc::new(String::from(s));
    let b = Rc::clone(&a);
    a.len() + b.len()
}

// --- strings, chars, casts, bitwise ---
pub fn f_str(s: &str) -> usize {
    let mut n = 0;
    for c in s.chars() {
        if c.is_ascii_alphabetic() {
            n += 1;
        }
    }
    n
}

pub fn f_cast(x: i64, y: f64) -> i64 {
    let a = x as i32 as i64;
    let b = y as i32 as i64;
    (a ^ b) & 0xff | (a << 2) | ((b as u64 >> 1) as i64)
}

// --- thread_local, raw ptr, unsafe, asm ---
thread_local! {
    static TLS_N: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

pub fn f_tls() -> u32 {
    TLS_N.with(|c| {
        c.set(c.get() + 1);
        c.get()
    })
}

pub fn f_raw(s: &mut i32) -> i32 {
    let p = s as *mut i32;
    let q = &raw const *p;
    unsafe { *p + *q }
}

pub fn f_asm(x: u64) -> u64 {
    let y: u64;
    unsafe {
        std::arch::asm!("mov {0}, {1}", out(reg) y, in(reg) x);
    }
    y
}

// --- let-underscore (PlaceMention), CopyForDeref shapes ---
pub fn f_underscore(v: String) -> usize {
    let _ = v.len();
    let b = Box::new(9i32);
    let d = *b;
    d as usize
}

// --- threads, map, entry API ---
pub fn f_thread_map() -> usize {
    let h = std::thread::spawn(|| 40usize + 2);
    let mut m: HashMap<String, usize> = HashMap::new();
    m.insert(String::from("a"), h.join().unwrap());
    *m.entry(String::from("b")).or_insert(1) += 1;
    m.values().sum()
}

// --- async fn (coroutine MIR: Yield/CoroutineDrop shapes) ---
pub async fn f_async(x: i32) -> i32 {
    let y = x + 1;
    y * 2
}

fn main() {
    println!("{}", f_match(Shape::Point(1, 2)));
    println!("{}", f_opt_mutate(&mut Some(0)));
    println!("{}", f_struct(Pt { x: 1, y: 2 }));
    println!("{}", f_tuple((1, String::from("ab"), true)));
    println!("{}", f_slice(&mut [1, 2, 3]));
    println!("{:?}", f_repeat());
    println!("{}", f_loops(&[1, 2]));
    println!("{:?}", f_closure(vec![1]));
    let _: (i32, i32) = f_generic(5i32);
    println!("{}", f_dyn(&Cat));
    println!("{}", f_static_dispatch(Dog));
    println!("{:?}", f_parse("21"));
    println!("{}", f_box(vec![1, 2]));
    println!("{}", f_rc("hi"));
    println!("{}", f_str("Ab1"));
    println!("{}", f_cast(-3, 2.5));
    println!("{}", f_tls());
    println!("{}", f_raw(&mut 4));
    println!("{}", f_asm(7));
    println!("{}", f_underscore(String::from("x")));
    println!("{}", f_thread_map());
}
