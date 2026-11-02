//! `atlas crop`: cut frames or a region out of a wide (or tall) picture
//! strip into a small PNG, so a picture is read as a crop rather than whole.
//!
//! ```text
//! atlas crop IN OUT [--frames A[-B] --of N [--axis x|y]] [--rect X,Y,W,H] [--max W[xH]]
//! ```
//!
//! * `--frames A-B --of N`: the strip is N equal frames along its longer
//!   side (or `--axis`); frames A to B (1-based, inclusive) are kept. Frame
//!   i spans `floor(len * (i - 1) / N)` to `floor(len * i / N)`.
//! * `--rect X,Y,W,H`: a region, in pixels of what the frames left (or of
//!   the whole picture), clipped to it.
//! * `--max W` or `--max WxH`: shrink by the smallest integer factor that
//!   fits, each pixel the mean of its block.
//!
//! IN is a PNG or a 24- or 32-bit BMP; OUT is always a PNG.

use std::process::ExitCode;

use crate::picture::{self, Image};

/// What to cut.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cut {
    /// First and last frame (1-based) and the frame count.
    pub frames: Option<(usize, usize, usize)>,
    /// Frames along x (true) or y; `None` is the longer side.
    pub along_x: Option<bool>,
    /// X, Y, W, H.
    pub rect: Option<(usize, usize, usize, usize)>,
    /// Largest width and height.
    pub max: Option<(usize, usize)>,
}

/// Applies a cut.
///
/// # Errors
///
/// A frame range outside `1..=N`.
pub fn apply(img: &Image, cut: &Cut) -> Result<Image, String> {
    let mut out = img.clone();
    if let Some((a, b, n)) = cut.frames {
        if n == 0 || a == 0 || a > b || b > n {
            return Err(format!("--frames {a}-{b} --of {n}: frames run from 1 to {n}"));
        }
        let x = cut.along_x.unwrap_or(out.width >= out.height);
        let len = if x { out.width } else { out.height };
        // In u128: `len * b` overflows usize for a large frame count.
        let at = |k: usize| (len as u128 * k as u128 / n as u128) as usize;
        let (lo, hi) = (at(a - 1), at(b));
        out = if x { out.crop(lo, 0, hi - lo, out.height) } else { out.crop(0, lo, out.width, hi - lo) };
    }
    if let Some((x, y, w, h)) = cut.rect {
        out = out.crop(x, y, w, h);
    }
    if let Some((mw, mh)) = cut.max {
        let k = out.width.div_ceil(mw.max(1)).max(out.height.div_ceil(mh.max(1))).max(1);
        out = out.shrink(k);
    }
    Ok(out)
}

fn nums(s: &str, n: usize, what: &str) -> Result<Vec<usize>, String> {
    let v: Vec<usize> = s.split([',', 'x', '-']).map(|p| p.trim().parse::<usize>()).collect::<Result<_, _>>().map_err(|_| format!("{what} {s:?}: expected numbers"))?;
    if v.len() == n {
        Ok(v)
    } else {
        Err(format!("{what} {s:?}: expected {n} numbers"))
    }
}

/// Parses the command line after `crop`.
///
/// # Errors
///
/// A usage message.
pub fn parse(args: &[String]) -> Result<(String, String, Cut), String> {
    let mut files = Vec::new();
    let mut cut = Cut::default();
    let mut range: Option<(usize, usize)> = None;
    let mut of: Option<usize> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--frames" => {
                let v = val("--frames")?;
                range = Some(match v.split_once('-') {
                    Some(_) => {
                        let n = nums(&v, 2, "--frames")?;
                        (n[0], n[1])
                    }
                    None => {
                        let n = nums(&v, 1, "--frames")?;
                        (n[0], n[0])
                    }
                });
            }
            "--of" => of = Some(nums(&val("--of")?, 1, "--of")?[0]),
            "--axis" => {
                cut.along_x = Some(match val("--axis")?.as_str() {
                    "x" => true,
                    "y" => false,
                    o => return Err(format!("--axis {o:?}: x or y")),
                })
            }
            "--rect" => {
                let n = nums(&val("--rect")?, 4, "--rect")?;
                cut.rect = Some((n[0], n[1], n[2], n[3]));
            }
            "--max" => {
                let v = val("--max")?;
                cut.max = Some(if v.contains('x') {
                    let n = nums(&v, 2, "--max")?;
                    (n[0], n[1])
                } else {
                    let n = nums(&v, 1, "--max")?;
                    (n[0], usize::MAX)
                });
            }
            f if !f.starts_with("--") => files.push(f.to_string()),
            o => return Err(format!("unknown option {o:?}\n{USAGE}")),
        }
    }
    match (range, of) {
        (Some((a, b)), Some(n)) => cut.frames = Some((a, b, n)),
        (None, None) => {}
        _ => return Err("--frames and --of go together".into()),
    }
    match <[String; 2]>::try_from(files) {
        Ok([i, o]) => Ok((i, o, cut)),
        Err(_) => Err(USAGE.into()),
    }
}

const USAGE: &str = "usage: atlas crop IN OUT [--frames A[-B] --of N [--axis x|y]] [--rect X,Y,W,H] [--max W[xH]]";

/// `atlas crop`.
///
/// # Errors
///
/// A usage error, an unreadable or unsupported picture, or an unwritable
/// output.
pub fn cli(args: Vec<String>) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let (input, output, cut) = parse(&args)?;
    let bytes = std::fs::read(&input).map_err(|e| format!("{input}: {e}"))?;
    let img = picture::decode(&bytes).map_err(|e| format!("{input}: {e}"))?;
    let out = apply(&img, &cut)?;
    let png = picture::encode_png(&out);
    std::fs::write(&output, &png).map_err(|e| format!("{output}: {e}"))?;
    println!("{output}: {}x{} from {}x{}, {} bytes", out.width, out.height, img.width, img.height, png.len());
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(w: usize, h: usize) -> Image {
        let mut rgba = Vec::new();
        for _y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[x as u8, 0, 0, 255]);
            }
        }
        Image { width: w, height: h, rgba }
    }

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn frames_follow_the_longer_side_and_the_rule() {
        let img = strip(100, 10);
        let c = apply(&img, &Cut { frames: Some((2, 3, 4)), ..Cut::default() }).unwrap();
        // Frames 2..3 of 4 span 25..75.
        assert_eq!((c.width, c.height), (50, 10));
        assert_eq!(c.rgba[0], 25);
        let tall = strip(10, 100);
        let c = apply(&tall, &Cut { frames: Some((4, 4, 4)), ..Cut::default() }).unwrap();
        assert_eq!((c.width, c.height), (10, 25));
        assert!(apply(&img, &Cut { frames: Some((3, 5, 4)), ..Cut::default() }).is_err());
    }

    #[test]
    fn rect_and_max_apply_after_frames() {
        let img = strip(100, 10);
        let c = apply(&img, &Cut { frames: Some((2, 2, 2)), rect: Some((10, 0, 20, 10)), max: Some((5, usize::MAX)), ..Cut::default() }).unwrap();
        // Frame 2 starts at x=50; the rect at 60..80; shrunk by 4 to 5 wide.
        assert_eq!((c.width, c.height), (5, 3));
        assert_eq!(c.rgba[0], 62);
    }

    #[test]
    fn a_huge_frame_count_does_not_overflow() {
        let img = Image { width: 4, height: 3, rgba: vec![9; 48] };
        let all = apply(&img, &Cut { frames: Some((1, usize::MAX, usize::MAX)), ..Cut::default() }).unwrap();
        assert_eq!((all.width, all.height), (4, 3));
        let last = apply(&img, &Cut { frames: Some((usize::MAX, usize::MAX, usize::MAX)), ..Cut::default() }).unwrap();
        assert!(last.width <= 1);
    }

    #[test]
    fn parses_the_command_line() {
        let (i, o, c) = parse(&args("a.png b.png --frames 2-3 --of 6 --axis y --rect 1,2,3,4 --max 320x200")).unwrap();
        assert_eq!((i.as_str(), o.as_str()), ("a.png", "b.png"));
        assert_eq!(c, Cut { frames: Some((2, 3, 6)), along_x: Some(false), rect: Some((1, 2, 3, 4)), max: Some((320, 200)) });
        assert_eq!(parse(&args("a b --frames 2 --of 3")).unwrap().2.frames, Some((2, 2, 3)));
        assert!(parse(&args("a b --frames 2")).is_err());
        assert!(parse(&args("a --max 3")).is_err());
    }
}
