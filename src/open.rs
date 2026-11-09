//! 画像ファイルと GIF ファイルを読み、ピンに渡せる形 (`Image` / `Recorded`) にする。

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use windows::Win32::Foundation::{E_FAIL, GENERIC_READ};
use windows::Win32::Graphics::Imaging::*;
use windows::core::{Error, HSTRING, Result};

use crate::capture::Image;
use crate::clip::FrameStore;
use crate::record::{Recorded, thumbnail};

fn fail(e: impl std::fmt::Display) -> Error {
    Error::new(E_FAIL, e.to_string())
}

/// 拡張子ではなく先頭 6 バイト (`GIF87a` / `GIF89a`) で GIF かどうかを決める。
pub fn is_gif(path: &Path) -> std::io::Result<bool> {
    let mut head = Vec::with_capacity(6);
    std::fs::File::open(path)?.take(6).read_to_end(&mut head)?;
    Ok(head == b"GIF87a" || head == b"GIF89a")
}

/// BGRA (α はストレート) を白の上に合成し、α を 255 にする。ピンは不透明で描く。
fn over_white(bgra: &mut [u8]) {
    for p in bgra.chunks_exact_mut(4) {
        let a = u32::from(p[3]);
        for c in &mut p[..3] {
            *c = ((u32::from(*c) * a + 255 * (255 - a) + 127) / 255) as u8;
        }
        p[3] = 255;
    }
}

/// WIC が読める静止画 (PNG・JPEG・BMP・WebP・1 フレームの GIF など) の先頭フレームを読む。
/// COM は呼び手のスレッドで初期化済みとする。
pub fn load_image(path: &Path) -> Result<Image> {
    let wic = &crate::ui::gfx().wic;
    unsafe {
        let decoder = wic.CreateDecoderFromFilename(
            &HSTRING::from(path.as_os_str()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?;
        let frame = decoder.GetFrame(0)?;
        let conv = wic.CreateFormatConverter()?;
        conv.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let (mut w, mut h) = (0, 0);
        conv.GetSize(&mut w, &mut h)?;
        let mut bgra = vec![0; (w * h * 4) as usize];
        conv.CopyPixels(std::ptr::null(), w * 4, &mut bgra)?;
        over_white(&mut bgra);
        Ok(Image { w, h, bgra })
    }
}

/// GIF の delay (1/100 秒)。0 と 1 は 10 として扱う (ブラウザと同じ)。
fn delay(cs: u16) -> Duration {
    Duration::from_millis(u64::from(if cs <= 1 { 10 } else { cs }) * 10)
}

/// 2 フレーム以上の GIF を論理画面の大きさで展開する。1 フレームなら `None`
/// (静止画として `load_image` で開く)。透明の画素は白で埋める。
/// 最後のフレームの表示時間は `end` (最後の開始時刻 + その delay) が持つ。
pub fn load_gif(path: &Path, thumb_size: (u32, u32)) -> Result<Option<Recorded>> {
    let bytes = std::fs::read(path).map_err(fail)?;

    // 1 周目: フレーム数と時刻だけ読む (画素は展開しない)。
    let mut opts = gif::DecodeOptions::new();
    opts.skip_frame_decoding(true);
    let mut d = opts.read_info(bytes.as_slice()).map_err(fail)?;
    let (mut ts, mut end) = (Vec::new(), Duration::ZERO);
    while let Some(f) = d.read_next_frame().map_err(fail)? {
        ts.push(end);
        end += delay(f.delay);
    }
    if ts.len() < 2 {
        return Ok(None);
    }
    let (w, h) = (usize::from(d.width()), usize::from(d.height()));
    if w == 0 || h == 0 {
        return Err(fail("GIF の論理画面が空"));
    }
    // キーフレームは平均の fps に当たる枚数ごと (約 1 秒ごと)。
    let per_sec = (ts.len() as f64 / end.as_secs_f64()).round().max(1.0) as usize;
    let mut store = FrameStore::new(w as u32, h as u32, per_sec);
    let mut thumbs = Vec::with_capacity(ts.len());

    // 2 周目: 各フレームを論理画面 (RGBA) に合成する。
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);
    let mut d = opts.read_info(bytes.as_slice()).map_err(fail)?;
    let mut screen = vec![0u8; w * h * 4];
    let mut out = vec![0u8; w * h * 4];
    for &t in &ts {
        let f = d
            .read_next_frame()
            .map_err(fail)?
            .ok_or_else(|| fail("GIF のフレームが途中で尽きた"))?;
        let saved = (f.dispose == gif::DisposalMethod::Previous).then(|| screen.clone());
        let (fx, fy, fw, fh) = (
            usize::from(f.left),
            usize::from(f.top),
            usize::from(f.width),
            usize::from(f.height),
        );
        // 論理画面からはみ出す部分は捨てる。
        let (cw, ch) = (fw.min(w.saturating_sub(fx)), fh.min(h.saturating_sub(fy)));
        for y in 0..ch {
            for x in 0..cw {
                let src = &f.buffer[(y * fw + x) * 4..][..4];
                if src[3] != 0 {
                    screen[((fy + y) * w + fx + x) * 4..][..4].copy_from_slice(src);
                }
            }
        }
        for (o, s) in out.chunks_exact_mut(4).zip(screen.chunks_exact(4)) {
            o.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
        }
        over_white(&mut out);
        store.push(&out, t);
        thumbs.push(thumbnail(&out, w as u32, h as u32, thumb_size));
        match f.dispose {
            gif::DisposalMethod::Background => {
                for y in 0..ch {
                    screen[((fy + y) * w + fx) * 4..][..cw * 4].fill(0);
                }
            }
            gif::DisposalMethod::Previous => screen = saved.unwrap(),
            _ => {}
        }
    }
    Ok(Some(Recorded {
        store,
        ts,
        end,
        thumbs,
        thumb_size,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gif::DisposalMethod::{Background, Keep, Previous};
    use std::borrow::Cow;
    use std::path::PathBuf;

    const R: [u8; 4] = [0, 0, 255, 255];
    const G: [u8; 4] = [0, 255, 0, 255];
    const B: [u8; 4] = [255, 0, 0, 255];
    const W: [u8; 4] = [255, 255, 255, 255];
    /// パレットの添字: 0 赤・1 緑・2 青・3 透明。
    const T: u8 = 3;

    /// (left, top, width, height, 添字, disposal, delay)
    type Spec = (u16, u16, u16, u16, Vec<u8>, gif::DisposalMethod, u16);

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("odile-open-{}-{name}", std::process::id()))
    }

    fn write_gif(name: &str, w: u16, h: u16, frames: Vec<Spec>) -> PathBuf {
        let mut bytes = Vec::new();
        {
            let pal = [255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0];
            let mut e = gif::Encoder::new(&mut bytes, w, h, &pal).unwrap();
            for (left, top, width, height, idx, dispose, delay) in frames {
                e.write_frame(&gif::Frame {
                    left,
                    top,
                    width,
                    height,
                    buffer: Cow::Owned(idx),
                    dispose,
                    delay,
                    transparent: Some(T),
                    ..Default::default()
                })
                .unwrap();
            }
        }
        let path = temp(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn frames(rec: &Recorded) -> Vec<Vec<u8>> {
        (0..rec.store.len())
            .map(|i| rec.store.frame(i).bgra)
            .collect()
    }

    fn px(p: &[[u8; 4]]) -> Vec<u8> {
        p.concat()
    }

    fn ms(v: &[u64]) -> Vec<Duration> {
        v.iter().map(|&m| Duration::from_millis(m)).collect()
    }

    #[test]
    fn gif_composites_frames_with_transparency_and_times() {
        let path = write_gif(
            "basic.gif",
            2,
            2,
            vec![
                (0, 0, 2, 2, vec![0, 0, 0, 0], Keep, 0),
                // 透明の画素は下を残す。
                (0, 0, 2, 2, vec![T, T, T, 1], Keep, 5),
                (0, 0, 1, 1, vec![2], Keep, 20),
            ],
        );
        let rec = load_gif(&path, (4, 2)).unwrap().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            frames(&rec),
            vec![px(&[R, R, R, R]), px(&[R, R, R, G]), px(&[B, R, R, G])]
        );
        // delay 0 は 100 ms。
        assert_eq!(rec.ts, ms(&[0, 100, 150]));
        assert_eq!(rec.end, Duration::from_millis(350));
        assert_eq!(rec.thumb_size, (4, 2));
        assert_eq!(rec.thumbs.len(), 3);
        assert!(rec.thumbs.iter().all(|t| t.len() == 4 * 2 * 4));
    }

    #[test]
    fn gif_disposal_background_and_previous() {
        // Background: そのフレームの矩形だけ透明 (白) に戻す。
        let path = write_gif(
            "bg.gif",
            2,
            2,
            vec![
                (0, 0, 2, 2, vec![0, 0, 0, 0], Keep, 10),
                (0, 0, 1, 1, vec![1], Background, 10),
                (1, 1, 1, 1, vec![2], Keep, 10),
            ],
        );
        let rec = load_gif(&path, (2, 2)).unwrap().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            frames(&rec),
            vec![px(&[R, R, R, R]), px(&[G, R, R, R]), px(&[W, R, R, B])]
        );

        // Previous: 描く前の画面に戻す。
        let path = write_gif(
            "prev.gif",
            2,
            2,
            vec![
                (0, 0, 2, 2, vec![0, 0, 0, 0], Keep, 10),
                (0, 0, 1, 1, vec![1], Previous, 10),
                (1, 1, 1, 1, vec![2], Keep, 10),
            ],
        );
        let rec = load_gif(&path, (2, 2)).unwrap().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            frames(&rec),
            vec![px(&[R, R, R, R]), px(&[G, R, R, R]), px(&[R, R, R, B])]
        );
    }

    fn com() {
        use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    }

    #[test]
    fn single_frame_gif_is_a_still_image() {
        com();
        let path = write_gif(
            "one.gif",
            2,
            2,
            vec![(0, 0, 2, 2, vec![0, 1, 2, T], Keep, 10)],
        );
        assert!(is_gif(&path).unwrap());
        assert!(load_gif(&path, (2, 2)).unwrap().is_none());
        let img = load_image(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!((img.w, img.h), (2, 2));
        assert_eq!(img.bgra, px(&[R, G, B, W]));
    }

    #[test]
    fn png_round_trips_through_write_png() {
        com();
        let src = Image {
            w: 3,
            h: 2,
            bgra: px(&[
                R,
                G,
                B,
                [10, 20, 30, 255],
                [10, 20, 30, 0],
                [200, 100, 50, 255],
            ]),
        };
        let path = temp("rt.png");
        crate::export::write_png(&path, &src).unwrap();
        assert!(!is_gif(&path).unwrap());
        let img = load_image(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!((img.w, img.h), (3, 2));
        // α 0 の画素は白。
        assert_eq!(
            img.bgra,
            px(&[R, G, B, [10, 20, 30, 255], W, [200, 100, 50, 255]])
        );
    }
}
