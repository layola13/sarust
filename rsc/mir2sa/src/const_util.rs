//! mir2sa::const_util.

pub fn array_elem_size(ty: &str) -> Option<usize> {
    match ty {
        "i8" | "u8" | "bool" => Some(1),
        "i32" | "u32" | "f32" => Some(4),
        "i64" | "u64" | "f64" | "usize" | "isize" => Some(8),
        _ => None,
    }
}

pub fn sa_scalar_ty(ty: &str) -> &str {
    match ty {
        "bool" => "u8",
        "usize" => "u64",
        "isize" => "i64",
        t => t,
    }
}

/// One array-init element to decimal text. Accepts both the `-Zunpretty` text
/// form (`1_i32`, `true`) and the driver's Debug form
/// (`Val(Scalar(0x00000001), i32)`). Anything else -> None (stay UNSUPPORTED).
pub fn const_array_elem(value: &str, ty: &str, size: usize) -> Option<String> {
    if ty == "bool" {
        return match value {
            "true" => Some("1".to_string()),
            "false" => Some("0".to_string()),
            // Rust bool Display ("true"/"false") is NOT a legal SA literal.
            _ => scalar_hex_value(value, "bool").map(|n| {
                if n != 0 {
                    "1".to_string()
                } else {
                    "0".to_string()
                }
            }),
        };
    }
    if let Some(us) = value.rfind('_') {
        let (num, suf) = (&value[..us], &value[us + 1..]);
        if suf == ty
            && !num.is_empty()
            && num.chars().all(|c| c.is_ascii_digit() || c == '-')
            && num.matches('-').count() <= 1
            && (!num.contains('-') || num.starts_with('-'))
        {
            return Some(num.to_string());
        }
        return None;
    }
    scalar_hex_value(value, ty).map(|n| {
        // Two's-complement wrap for signed types (e.g. 0xFFFFFFFF -> -1).
        let bits = size * 8;
        if matches!(ty, "i8" | "i16" | "i32" | "i64" | "isize") && bits < 128 && (n >> (bits - 1)) & 1 == 1 {
            ((n as i128).wrapping_sub(1 << bits)).to_string()
        } else {
            n.to_string()
        }
    })
}

/// `Val(Scalar(0xHEX), TY)` -> integer iff TY matches; None otherwise.
pub fn scalar_hex_value(value: &str, ty: &str) -> Option<u128> {
    let v = value.strip_prefix("Val(Scalar(0x")?;
    let comma = v.find(", ")?;
    let hex = v[..comma].trim_end_matches(')');
    let suffix = v[comma + 2..].trim_end_matches(')');
    if suffix != ty {
        return None;
    }
    // Guard: hex payload only, no nested Debug text.
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) || hex.is_empty() {
        return None;
    }
    u128::from_str_radix(hex, 16).ok()
}

/// `[elem; len]` const-fill plan -> (decimal byte value, total bytes).
/// Accepts text (`7_u8`, `8`) and driver-Debug (`Val(Scalar(0x07), u8)`)
/// shapes. Element must be a single byte value (u8/i8), like the
/// `sa_mem_set(&buf, 0, bytes)` sites in `sci/sa_std/alloc/vec.sa`.
pub fn repeat_plan(op_value: &str, len_text: &str) -> Option<(String, usize)> {
    let (val, size) = repeat_elem(op_value)?;
    let n = repeat_len(len_text)?;
    n.checked_mul(size).map(|total| (val, total))
}

pub fn repeat_elem(op_value: &str) -> Option<(String, usize)> {
    // Text form `7_u8`.
    if let Some(us) = op_value.rfind('_') {
        let (num, suf) = (&op_value[..us], &op_value[us + 1..]);
        if matches!(suf, "u8" | "i8") && !num.is_empty()
            && num.chars().all(|c| c.is_ascii_digit() || c == '-')
        {
            let v: i128 = num.parse().ok()?;
            return byte_value(v, suf).map(|b| (b.to_string(), 1));
        }
        return None;
    }
    // Driver-Debug form `Val(Scalar(0x07), u8)`.
    for ty in ["u8", "i8"] {
        if let Some(n) = scalar_hex_value(op_value, ty) {
            let v = if ty == "i8" && n >= 128 { (n as i128) - 256 } else { n as i128 };
            return byte_value(v, ty).map(|b| (b.to_string(), 1));
        }
    }
    None
}

pub fn byte_value(v: i128, ty: &str) -> Option<i128> {
    match ty {
        "u8" if (0..=255).contains(&v) => Some(v),
        "i8" if (-128..=127).contains(&v) => Some(v),
        _ => None,
    }
}

pub fn repeat_len(len_text: &str) -> Option<usize> {
    let t = len_text.trim();
    if let Ok(n) = t.parse::<usize>() {
        return Some(n);
    }
    // `<num>_<tysuffix>` (text `8`, pretty `8_usize`): digits before `_`.
    if let Some(us) = t.find('_') {
        if let Ok(n) = t[..us].parse::<usize>() {
            if t[us + 1..].trim_end_matches(')').chars().all(|c| c.is_ascii_alphanumeric()) {
                return Some(n);
            }
        }
    }
    // Driver-Debug scalar: `Val(Scalar(0x08), usize)`.
    scalar_hex_value(t, "usize").and_then(|n| usize::try_from(n).ok())
}

/// Infer `(ty, size)` for a Const aggregate element.
/// Accepts text form (`1_i32`) and driver-Debug form
/// (`Val(Scalar(0x00000001), i32)`); bool true/false included.
pub fn const_elem_ty(value: &str) -> Option<(&'static str, usize)> {
    let v = value.trim();
    if v == "true" || v == "false" {
        return Some(("bool", 1));
    }
    if let Some(us) = v.rfind('_') {
        let suf = &v[us + 1..];
        // Text form suffix is a bare type name (no parens/commas/spaces).
        if !suf.is_empty()
            && suf.chars().all(|c| c.is_ascii_alphanumeric())
            && !v[..us].is_empty()
        {
            let size = match suf {
                "bool" | "u8" | "i8" => 1,
                "u16" | "i16" => 2,
                "u32" | "i32" | "f32" => 4,
                "u64" | "i64" | "usize" | "isize" | "f64" => 8,
                _ => return None,
            };
            return Some((match suf {
                "bool" => "bool", "u8" => "u8", "i8" => "i8",
                "u16" => "u16", "i16" => "i16",
                "u32" => "u32", "i32" => "i32", "f32" => "f32",
                "u64" => "u64", "i64" => "i64", "usize" => "usize",
                "isize" => "isize", "f64" => "f64",
                _ => return None,
            }, size));
        }
    }
    // Driver-Debug form: `Val(Scalar(0xHEX), TY)`.
    if let Some(rest) = v.strip_prefix("Val(Scalar(0x") {
        if let Some(comma) = rest.find(", ") {
            let suffix = rest[comma + 2..].trim_end_matches(')');
            let size = match suffix {
                "bool" | "u8" | "i8" => 1,
                "u16" | "i16" => 2,
                "u32" | "i32" | "f32" => 4,
                "u64" | "i64" | "usize" | "isize" | "f64" => 8,
                _ => return None,
            };
            // Leak a 'static str would be wrong; return size only via caller map.
            // Instead match again to a static slice (suffix is not 'static).
            let ty: &'static str = match suffix {
                "bool" => "bool", "u8" => "u8", "i8" => "i8",
                "u16" => "u16", "i16" => "i16",
                "u32" => "u32", "i32" => "i32", "f32" => "f32",
                "u64" => "u64", "i64" => "i64", "usize" => "usize",
                "isize" => "isize", "f64" => "f64",
                _ => return None,
            };
            return Some((ty, size));
        }
    }
    // `Val(Scalar(0x01), bool)` single-hex bool is covered above via suffix;
    // bare `0`/`1` without suffix cannot be typed -> caller keeps UNSUPPORTED.
    None
}
