//! The few Python behaviours the ported tools' output depends on, so a port
//! prints what its script printed, byte for byte: float `repr`, `round`,
//! `sum` (CPython's compensated float sum), `json.dumps` lengths, text-mode
//! line splitting, `float()` and `int()` parsing, and `str.isspace`.

/// Python's `repr(x)` of a float: the shortest digits that round-trip, in
/// fixed notation when the decimal exponent is in -4..16, else scientific
/// with a signed, at least two-digit exponent (`1e-05`, `1.5e+16`).
pub fn repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    // `{:e}` gives the shortest round-trip digits: "-1.2345e-5", "1e16".
    let s = format!("{x:e}");
    let (mant, exp) = s.split_once('e').expect("{:e} has an exponent");
    let exp: i32 = exp.parse().expect("{:e} exponent is an integer");
    let (neg, mant) = match mant.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mant),
    };
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        if exp < 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-exp - 1) as usize));
            out.push_str(&digits);
        } else {
            let point = exp as usize + 1;
            if digits.len() <= point {
                out.push_str(&digits);
                out.extend(std::iter::repeat_n('0', point - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..point]);
                out.push('.');
                out.push_str(&digits[point..]);
            }
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push_str(&format!("e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs()));
    }
    out
}

/// Python's `round(x, nd)` for a float: the exact binary value rounded to
/// `nd` decimals, ties to even, read back as the nearest float.
pub fn round(x: f64, nd: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    // Rust's fixed-precision formatting rounds the exact value, ties to even,
    // as CPython's `_Py_dg_dtoa` mode 3 does.
    format!("{x:.nd$}").parse().expect("a formatted float parses")
}

/// Python's `round(x)` with no digits: the nearest integer, ties to even.
pub fn round_int(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// A Python number as `sum()` sees it: an int or a float.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Num {
    /// A Python `int` (wide enough for every count the tools make).
    Int(i128),
    /// A Python `float`.
    Float(f64),
}

impl Num {
    /// The value as a float (`float(x)`).
    pub fn f(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(x) => x,
        }
    }
    /// `self + o` with Python's int/float promotion.
    pub fn add(self, o: Num) -> Num {
        match (self, o) {
            (Num::Int(a), Num::Int(b)) => Num::Int(a + b),
            (a, b) => Num::Float(a.f() + b.f()),
        }
    }
}

/// The range of a C `long`: CPython's `sum()` adds an int that fits one
/// straight onto the float total, and finishes the compensation before
/// adding one that does not. `long` is 32 bits on Windows, 64 elsewhere,
/// so the script's last bits depended on the platform; so do these.
fn fits_c_long(i: i128) -> bool {
    if cfg!(windows) {
        i32::try_from(i).is_ok()
    } else {
        i64::try_from(i).is_ok()
    }
}

/// CPython 3.12+'s `sum(xs)` (start 0): ints summed exactly until the first
/// float, then floats by Neumaier's compensated sum; an int inside the float
/// run is added uncompensated when it fits a C long, else the compensation
/// is folded in first and restarted.
pub fn sum(xs: impl IntoIterator<Item = Num>) -> Num {
    let mut it = xs.into_iter();
    let mut acc: i128 = 0;
    let first = loop {
        match it.next() {
            None => return Num::Int(acc),
            Some(Num::Int(i)) => acc += i,
            Some(Num::Float(x)) => break acc as f64 + x,
        }
    };
    let (mut f, mut c) = (first, 0.0f64);
    for x in it {
        match x {
            Num::Float(x) => {
                let t = f + x;
                if f.abs() >= x.abs() {
                    c += (f - t) + x;
                } else {
                    c += (x - t) + f;
                }
                f = t;
            }
            Num::Int(i) if fits_c_long(i) => f += i as f64,
            Num::Int(i) => {
                if c != 0.0 && c.is_finite() {
                    f += c;
                }
                f += i as f64;
                c = 0.0;
            }
        }
    }
    if c != 0.0 && c.is_finite() {
        f += c;
    }
    Num::Float(f)
}

/// `statistics.median` of floats: the middle value, or the mean of the two
/// middle values.
pub fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.total_cmp(b));
    let n = xs.len();
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    }
}

/// The lines of a file read in Python's text mode: `\r\n` and a lone `\r`
/// end a line as `\n` does, and the final line need not end.
pub fn text_lines(s: &str) -> Vec<String> {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut out: Vec<String> = s.split_inclusive('\n').map(str::to_string).collect();
    if out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

/// Python's `str.isspace()` for one character.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python's `float(s)` for the forms a TSV holds (surrounding whitespace,
/// a sign, `inf` and `nan` in any case).
pub fn parse_float(s: &str) -> Option<f64> {
    s.trim_matches(is_space).parse().ok()
}

/// Python's `int(s)` for the forms a TSV holds (surrounding whitespace, a
/// sign).
pub fn parse_int(s: &str) -> Option<i64> {
    s.trim_matches(is_space).parse().ok()
}

/// The length in characters of `json.dumps(v)` with its defaults: `", "`
/// and `": "` separators, non-ASCII escaped as `\uXXXX` (a surrogate pair
/// beyond the BMP), floats as `repr`.
pub fn dumps_len(v: &atlas::json::Value) -> usize {
    use atlas::json::{Number, Value};
    fn str_len(s: &str) -> usize {
        2 + s
            .chars()
            .map(|c| match c {
                '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
                c if (c as u32) < 0x20 => 6,
                // Escaped: everything outside ' '..='~', so DEL too.
                c if (c as u32) < 0x7f => 1,
                c if (c as u32) > 0xffff => 12,
                _ => 6,
            })
            .sum::<usize>()
    }
    match v {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(Number::I(i)) => i.to_string().len(),
        Value::Number(Number::U(u)) => u.to_string().len(),
        Value::Number(Number::F(x)) => match repr(*x).as_str() {
            "inf" => 8,
            "-inf" => 9,
            "nan" => 3,
            r => r.len(),
        },
        Value::String(s) => str_len(s),
        Value::Array(a) => 2 + a.iter().map(dumps_len).sum::<usize>() + 2 * a.len().saturating_sub(1),
        Value::Object(m) => 2 + m.iter().map(|(k, v)| str_len(k) + 2 + dumps_len(v)).sum::<usize>() + 2 * m.len().saturating_sub(1),
    }
}

/// Python truthiness of a JSON value.
pub fn truthy(v: Option<&atlas::json::Value>) -> bool {
    use atlas::json::Value;
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(m)) => !m.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_matches_python() {
        for (x, want) in [
            (0.0, "0.0"),
            (0.35, "0.35"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (-2.5, "-2.5"),
            (123456789012345678.0, "1.2345678901234568e+17"),
            (0.1 + 0.2, "0.30000000000000004"),
        ] {
            assert_eq!(repr(x), want, "{x}");
        }
    }

    #[test]
    fn round_ties_to_even_on_the_exact_value() {
        // 0.125 and 0.375 are exact binary ties; 2.675 is just below its tie.
        assert_eq!(round(0.125, 2), 0.12);
        assert_eq!(round(0.375, 2), 0.38);
        assert_eq!(round(2.675, 2), 2.67);
        assert_eq!(round_int(2.5), 2);
        assert_eq!(round_int(3.5), 4);
    }

    #[test]
    fn sum_compensates_floats_like_cpython() {
        // Values checked against CPython 3.13: sum([0.1] * 10) == 1.0, and
        // the naive left fold is 0.9999999999999999.
        assert_eq!(sum(std::iter::repeat_n(Num::Float(0.1), 10)), Num::Float(1.0));
        assert_eq!(sum([Num::Int(2), Num::Int(3)]), Num::Int(5));
    }

    #[test]
    fn text_lines_split_like_python_text_mode() {
        assert_eq!(text_lines("a\r\nb\rc\nd"), ["a\n", "b\n", "c\n", "d"]);
        assert_eq!(text_lines("a\n"), ["a\n"]);
    }

    #[test]
    fn dumps_len_counts_python_escapes() {
        let v = atlas::json::parse("{\"a\": [1, 2.0, \"\u{e9}\u{1f600}\", null], \"b\\n\": true}").unwrap();
        // json.dumps writes the e-acute as a six-character escape, the emoji as a twelve-character
        // surrogate pair and the newline as two: 7 + 3 + 5 + 20 + 7 + 2 + 5 + 6 + 1 characters.
        assert_eq!(dumps_len(&v), 56);
    }
}
