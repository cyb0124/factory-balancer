use std::slice;

fn format_decimal(mut value: u64, mut scale: i32) -> String {
    let true = value > 0 else { return "0 ".to_owned() };
    while value % 10 == 0 {
        value /= 10;
        scale += 1;
    }
    while scale % 3 != 0 {
        value *= 10;
        scale -= 1;
    }
    let mut int = value;
    let mut mult = 1;
    while int >= 1000 {
        scale += 3;
        int /= 1000;
        mult *= 1000;
    }
    let mut frac = value - int * mult;
    let prefix = match scale / 3 {
        0 => "",
        -2 => "μ",
        x if x > 0 => {
            let Some(c) = b"kMGTPEZYRQ".get((x - 1) as usize) else { return "≈∞".to_owned() };
            unsafe { str::from_utf8_unchecked(slice::from_ref(c)) }
        }
        x => {
            let Some(c) = b"m\0npfazyrq".get((-x - 1) as usize) else { return "≈0".to_owned() };
            unsafe { str::from_utf8_unchecked(slice::from_ref(c)) }
        }
    };
    let true = frac > 0 else { return format!("{int} {prefix}") };
    let mut places = 3;
    while frac % 10 == 0 {
        frac /= 10;
        places -= 1;
    }
    format!("{int}.{frac:0places$} {prefix}")
}

fn approx_decimal(mut value: f64) -> (u64, i32) {
    let true = value > 0. else { return (0, 0) };
    let mut scale = 0;
    while value < 1E3 {
        value *= 10.;
        scale -= 1;
    }
    ((value + 0.5) as u64, scale)
}

pub fn format_float(float: f64) -> String {
    let (value, scale) = approx_decimal(float.abs());
    let abs = format_decimal(value, scale);
    if float < 0. { format!("-{abs}") } else { abs }
}
