//! Frosted glass for the small windows that sit on the desktop: the tray menu
//! and the notification pop-up, under Liquid Glass.
//!
//! A page can't blur what is behind its window (`backdrop-filter` only sees
//! the page itself). Windows' own acrylic can, but on Windows 10 it fills the
//! whole window rectangle and ignores a window region, so a rounded menu came
//! out with square frost around rounded content: a double corner.
//!
//! So the frost is made here instead. Just before the window appears, the
//! part of the screen it is about to cover is captured, shrunk and blurred,
//! and handed to the page as an image to paint behind its content. The page
//! draws the rounded corners itself, anti-aliased, so there is one clean edge.
//! The snapshot doesn't follow what moves behind the window afterwards, which
//! for a menu that is open a few seconds nobody notices.

/// How much the capture is shrunk before blurring. The page scales it back
/// up with smoothing, which is most of the blur.
const SHRINK: u32 = 6;
/// Box blur passes over the shrunk image, which round the result off into a
/// gaussian-like frost.
const BLUR_PASSES: usize = 3;
const BLUR_RADIUS: i32 = 2;

/// The blurred screen under a rectangle (physical pixels), as a `data:` URL,
/// or `None` if it couldn't be captured. Call it while the window that will
/// cover the rectangle is still hidden, or it captures that window.
pub fn backdrop(x: i32, y: i32, w: i32, h: i32) -> Option<String> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let pixels = capture(x, y, w, h)?;
    let (sw, sh) = (((w as u32) / SHRINK).max(1), ((h as u32) / SHRINK).max(1));
    let mut small = shrink(&pixels, w as u32, h as u32, sw, sh);
    for _ in 0..BLUR_PASSES {
        box_blur(&mut small, sw, sh);
    }
    let img = image::RgbImage::from_raw(sw, sh, small)?;
    let mut jpeg = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg).ok()?;
    Some(format!("data:image/jpeg;base64,{}", base64(&jpeg)))
}

/// The screen's pixels under the rectangle, as RGB rows top to bottom.
#[cfg(windows)]
fn capture(x: i32, y: i32, w: i32, h: i32) -> Option<Vec<u8>> {
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, SRCCOPY,
    };
    // SAFETY: every handle made here is released on the way out; the buffer
    // is exactly the size GetDIBits writes for a 32-bit top-down bitmap.
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return None;
        }
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp);
        let copied = BitBlt(mem, 0, 0, w, h, screen, x, y, SRCCOPY | CAPTUREBLT) != 0;

        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        SelectObject(mem, old);
        let lines = if copied {
            GetDIBits(mem, bmp, 0, h as u32, bgra.as_mut_ptr().cast(), &mut info, DIB_RGB_COLORS)
        } else {
            0
        };
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);
        if lines != h {
            return None;
        }
        // BGRA to RGB. `as_chunks` over a constant size hands back real
        // `[u8; 4]`s, so the indices below are checked once by the compiler
        // rather than on every pixel.
        Some(
            bgra.as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[2], p[1], p[0]])
                .collect(),
        )
    }
}

#[cfg(not(windows))]
fn capture(_x: i32, _y: i32, _w: i32, _h: i32) -> Option<Vec<u8>> {
    None
}

/// Area-average an RGB image down to `sw` x `sh`.
fn shrink(src: &[u8], w: u32, h: u32, sw: u32, sh: u32) -> Vec<u8> {
    let mut out = vec![0u8; (sw * sh * 3) as usize];
    for oy in 0..sh {
        let (y0, y1) = (oy * h / sh, ((oy + 1) * h / sh).max(oy * h / sh + 1));
        for ox in 0..sw {
            let (x0, x1) = (ox * w / sw, ((ox + 1) * w / sw).max(ox * w / sw + 1));
            let mut sum = [0u32; 3];
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = ((y * w + x) * 3) as usize;
                    for c in 0..3 {
                        sum[c] += src[i + c] as u32;
                    }
                }
            }
            let n = (y1 - y0) * (x1 - x0);
            let o = ((oy * sw + ox) * 3) as usize;
            for c in 0..3 {
                out[o + c] = (sum[c] / n) as u8;
            }
        }
    }
    out
}

/// One horizontal and one vertical box blur pass, clamped at the edges.
fn box_blur(img: &mut [u8], w: u32, h: u32) {
    let (w, h) = (w as i32, h as i32);
    let at = |x: i32, y: i32| ((y.clamp(0, h - 1) * w + x.clamp(0, w - 1)) * 3) as usize;
    let n = (BLUR_RADIUS * 2 + 1) as u32;
    for horizontal in [true, false] {
        let src = img.to_vec();
        for y in 0..h {
            for x in 0..w {
                let mut sum = [0u32; 3];
                for d in -BLUR_RADIUS..=BLUR_RADIUS {
                    let i = if horizontal { at(x + d, y) } else { at(x, y + d) };
                    for c in 0..3 {
                        sum[c] += src[i + c] as u32;
                    }
                }
                let o = at(x, y);
                for c in 0..3 {
                    img[o + c] = (sum[c] / n) as u8;
                }
            }
        }
    }
}

/// Standard base64 with padding.
pub(crate) fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn shrinking_averages_each_block() {
        // 2x2 black and white checker shrunk to one pixel is mid grey.
        let src = [0, 0, 0, 255, 255, 255, 255, 255, 255, 0, 0, 0];
        assert_eq!(super::shrink(&src, 2, 2, 1, 1), vec![127, 127, 127]);
    }
}
