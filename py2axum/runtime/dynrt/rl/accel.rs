//! Pure Python paths of reportlab/lib/rl_accel.py: fp_str, escapePDF, asciiBase85Encode,
//! unicode2T1 and the T1 stringWidth.

use super::fonts::{self, FontId};

/// Python's `log(x)/log(10)` (not log10: the rounding differs on exact powers of ten).
fn log_10(x: f64) -> f64 {
    x.ln() / 10f64.ln()
}

/// One number as `_py_fp_str` writes it.
pub fn fp1(i: f64) -> String {
    let sa = i.abs();
    if sa <= 1e-7 {
        return "0".to_string();
    }
    let l: usize = if sa <= 1.0 {
        6
    } else {
        // min(max(0, 6 - int(log10(sa))), 6) ; int() truncates toward zero
        let v = 6 - (log_10(sa) as i64);
        v.clamp(0, 6) as usize
    };
    let mut n = format!("{:.*}", l, i);
    if l > 0 {
        let b = n.as_bytes();
        let mut j = b.len();
        while j > 0 {
            j -= 1;
            if b[j] != b'0' {
                if b[j] != b'.' {
                    j += 1;
                }
                break;
            }
        }
        n.truncate(j);
    }
    if n.as_bytes()[0] != b'0' || n.len() == 1 {
        n
    } else {
        n[1..].to_string()
    }
}

/// `fp_str(*a)`: space separated numbers.
pub fn fp_str(a: &[f64]) -> String {
    let v: Vec<String> = a.iter().map(|&x| fp1(x)).collect();
    v.join(" ")
}

/// `escapePDF` of a byte string (output is ASCII).
pub fn escape_pdf(s: &[u8]) -> String {
    let mut r = String::with_capacity(s.len() + 8);
    for &c in s {
        if c < 32 || c >= 127 {
            r.push_str(&format!("\\{:03o}", c));
        } else if c == b'\\' || c == b'(' || c == b')' {
            r.push('\\');
            r.push(c as char);
        } else {
            r.push(c as char);
        }
    }
    r
}

/// `asciiBase85Encode` (with the `~>` terminator).
pub fn ascii_base85_encode(input: &[u8]) -> String {
    let whole = input.len() / 4;
    let rem = input.len() % 4;
    let mut out = String::with_capacity(input.len() * 5 / 4 + 8);
    for w in 0..whole {
        let b = &input[w * 4..w * 4 + 4];
        let num = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        if num == 0 {
            out.push('z');
        } else {
            push85(&mut out, num, 5);
        }
    }
    if rem > 0 {
        let mut last = [0u8; 4];
        last[..rem].copy_from_slice(&input[whole * 4..]);
        push85(&mut out, u32::from_be_bytes(last), rem + 1);
    }
    out.push_str("~>");
    out
}

fn push85(out: &mut String, num: u32, n: usize) {
    let mut t = num;
    let mut c = [0u8; 5];
    for k in (0..5).rev() {
        c[k] = (t % 85) as u8 + 33;
        t /= 85;
    }
    for &x in &c[..n] {
        out.push(x as char);
    }
}

/// `unicode2T1(text, [font] + font.substitutionFonts)`: (font, encoded bytes) runs.
pub fn unicode2t1(text: &str, font: FontId) -> Vec<(FontId, Vec<u8>)> {
    let chars: Vec<char> = text.chars().collect();
    let mut fonts_list = vec![font];
    fonts_list.extend_from_slice(fonts::substitution_fonts(font));
    let mut r = Vec::new();
    u2t1(&chars, &fonts_list, &mut r);
    r
}

fn u2t1(mut utext: &[char], fonts_list: &[FontId], r: &mut Vec<(FontId, Vec<u8>)>) {
    let font = fonts_list[0];
    let rest = &fonts_list[1..];
    while !utext.is_empty() {
        // the first maximal run of unencodable characters, as CPython's charmap encoder reports it
        let i0 = utext.iter().position(|&c| fonts::encode_char(font, c).is_none());
        let Some(i0) = i0 else {
            r.push((font, utext.iter().map(|&c| fonts::encode_char(font, c).unwrap()).collect()));
            break;
        };
        let mut il = i0;
        while il < utext.len() && fonts::encode_char(font, utext[il]).is_none() {
            il += 1;
        }
        if i0 > 0 {
            r.push((font, utext[..i0].iter().map(|&c| fonts::encode_char(font, c).unwrap()).collect()));
        }
        if !rest.is_empty() {
            u2t1(&utext[i0..il], rest, r);
        } else {
            let nd = fonts::notdef_font(font);
            r.push((nd, vec![fonts::NOTDEF_CHAR; il - i0]));
        }
        utext = &utext[il..];
    }
}

/// `instanceStringWidthT1(font, text, size)`.
pub fn string_width(text: &str, font: FontId, size: f64) -> f64 {
    let mut total: i64 = 0;
    for (f, t) in unicode2t1(text, font) {
        let w = &fonts::font(f).widths;
        total += t.iter().map(|&b| w[b as usize] as i64).sum::<i64>();
    }
    total as f64 * 0.001 * size
}
