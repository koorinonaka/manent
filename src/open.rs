//! 画像ファイルと GIF ファイルを読み、ピンに渡せる形 (`Image` / `Recorded`) にする。

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use windows::Win32::Foundation::{E_FAIL, GENERIC_READ};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
use windows::Win32::System::Variant::VT_UI2;
use windows::core::{Error, HSTRING, Interface, Result, w};

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
        if a == 255 {
            continue;
        }
        for c in &mut p[..3] {
            *c = ((u32::from(*c) * a + 255 * (255 - a) + 127) / 255) as u8;
        }
        p[3] = 255;
    }
}

/// `screen` (RGBA、α はストレート) の矩形 (x, y, 幅, 高さ) を、白の上に合成した BGRA にして `out` へ写す。
fn to_out(screen: &[u8], out: &mut [u8], w: usize, (x, y, cw, ch): (usize, usize, usize, usize)) {
    for y in y..y + ch {
        let span = (y * w + x) * 4..(y * w + x + cw) * 4;
        let o = &mut out[span.clone()];
        for (o, s) in o.chunks_exact_mut(4).zip(screen[span].chunks_exact(4)) {
            o.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
        }
        over_white(o);
    }
}

/// EXIF の Orientation (タグ 274)。WIC の写真のポリシー名 `System.Photo.Orientation` で引く。
/// この名前は JPEG (`/app1/ifd/{ushort=274}`) と TIFF (`/ifd/{ushort=274}`) の両方で通る (テストで確かめた)。
/// メタデータが無い (PNG・BMP など)・タグが無い・`VT_UI2` でないときは 1。
fn orientation(frame: &IWICBitmapFrameDecode) -> u16 {
    unsafe {
        let Ok(query) = frame.GetMetadataQueryReader() else {
            return 1;
        };
        let mut v = PROPVARIANT::default();
        let o = match query.GetMetadataByName(w!("System.Photo.Orientation"), &mut v) {
            Ok(()) if v.Anonymous.Anonymous.vt == VT_UI2 => v.Anonymous.Anonymous.Anonymous.uiVal,
            _ => 1,
        };
        // windows の `PROPVARIANT` は Drop で放さない。
        let _ = PropVariantClear(&mut v);
        o
    }
}

/// EXIF の Orientation (タグ 274) を、正しい向きに直す WIC の変換にする。WIC は反転してから回す (5 と 7 はテストで確かめた)。
/// 知らない値は 1 (そのまま) と同じ。
fn exif_transform(orientation: u16) -> WICBitmapTransformOptions {
    let or = |a: WICBitmapTransformOptions, b: WICBitmapTransformOptions| {
        WICBitmapTransformOptions(a.0 | b.0)
    };
    match orientation {
        2 => WICBitmapTransformFlipHorizontal,
        3 => WICBitmapTransformRotate180,
        4 => WICBitmapTransformFlipVertical,
        5 => or(
            WICBitmapTransformRotate270,
            WICBitmapTransformFlipHorizontal,
        ),
        6 => WICBitmapTransformRotate90,
        7 => or(WICBitmapTransformRotate90, WICBitmapTransformFlipHorizontal),
        8 => WICBitmapTransformRotate270,
        _ => WICBitmapTransformRotate0,
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
        let source: IWICBitmapSource = match exif_transform(orientation(&frame)) {
            t if t == WICBitmapTransformRotate0 => frame.cast()?,
            t => {
                // 回転器はデコーダから直に読むと 1 画素ごとに走査線を読み直す。先に展開しておく。
                let bitmap = wic.CreateBitmapFromSource(&frame, WICBitmapCacheOnLoad)?;
                let rot = wic.CreateBitmapFlipRotator()?;
                rot.Initialize(&bitmap, t)?;
                rot.cast()?
            }
        };
        let conv = wic.CreateFormatConverter()?;
        conv.Initialize(
            &source,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let (mut w, mut h) = (0, 0);
        conv.GetSize(&mut w, &mut h)?;
        let mut bgra = vec![0; w as usize * h as usize * 4];
        conv.CopyPixels(std::ptr::null(), w * 4, &mut bgra)?;
        over_white(&mut bgra);
        Ok(Image { w, h, bgra })
    }
}

/// GIF の論理画面の一辺の上限。ピンのビットマップ (D2D) が一辺 16384 までしか作れない。
const MAX_GIF_SIDE: usize = 16384;
/// 論理画面 1 枚 (BGRA) の上限 1 GiB。展開中はこの大きさの面を 3 枚 (`screen`・`out`・差分の基準) 持つ。
/// 一辺の上限どうしの積 (16384² × 4) と同じ値で、どちらかが緩んでも 1 枚 1 GiB を超えない。
const MAX_GIF_SCREEN_BYTES: usize = 1 << 30;

/// GIF の delay (1/100 秒)。0 と 1 は 10 として扱う (ブラウザと同じ)。
fn delay(cs: u16) -> Duration {
    Duration::from_millis(u64::from(if cs <= 1 { 10 } else { cs }) * 10)
}

/// 2 フレーム以上の GIF を論理画面の大きさで展開する。1 フレームなら `None`
/// (静止画として `load_image` で開く)。透明の画素は白で埋める。
/// 最後のフレームの表示時間は `end` (最後の開始時刻 + その delay) が持つ。
pub fn load_gif(path: &Path, thumb_size: (u32, u32)) -> Result<Option<Recorded>> {
    crate::insight_scope!("open_load_gif");
    let bytes = std::fs::read(path).map_err(fail)?;

    // 1 周目: フレーム数と時刻だけ読む (画素は展開しない)。
    let mut opts = gif::DecodeOptions::new();
    opts.skip_frame_decoding(true);
    let mut d = opts.read_info(bytes.as_slice()).map_err(fail)?;
    // 論理画面の大きさは展開の前に確かめる。`screen`・`out`・`FrameStore` の差分の基準が各 w*h*4 を取り、
    // gif crate の `memory_limit` はフレームにしか掛からない (65535×65535 のヘッダで 1 つ約 17 GB)。
    let (w, h) = (usize::from(d.width()), usize::from(d.height()));
    if w > MAX_GIF_SIDE || h > MAX_GIF_SIDE || w * h * 4 > MAX_GIF_SCREEN_BYTES {
        return Err(fail(format!("GIF の論理画面が大きすぎる ({w}x{h})")));
    }
    let (mut ts, mut end) = (Vec::new(), Duration::ZERO);
    while let Some(f) = d.read_next_frame().map_err(fail)? {
        ts.push(end);
        end += delay(f.delay);
    }
    if ts.len() < 2 {
        return Ok(None);
    }
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
    // `screen` を白の上に合成した BGRA。変わった矩形だけ写し直す (透明の画面は白)。
    let mut out = vec![255u8; w * h * 4];
    // 前のフレームの dispose で `screen` を戻した矩形。
    let mut restored = None;
    for &t in &ts {
        let f = d
            .read_next_frame()
            .map_err(fail)?
            .ok_or_else(|| fail("GIF のフレームが途中で尽きた"))?;
        let (fx, fy, fw, fh) = (
            usize::from(f.left),
            usize::from(f.top),
            usize::from(f.width),
            usize::from(f.height),
        );
        // 論理画面からはみ出す部分は捨てる。
        let (cw, ch) = (fw.min(w.saturating_sub(fx)), fh.min(h.saturating_sub(fy)));
        let rect = (cw > 0 && ch > 0).then_some((fx, fy, cw, ch));
        let row = |y: usize| ((fy + y) * w + fx) * 4..((fy + y) * w + fx + cw) * 4;
        // Previous は描く前の矩形の行だけ取っておく。
        let saved: Option<Vec<u8>> = (rect.is_some() && f.dispose == gif::DisposalMethod::Previous)
            .then(|| (0..ch).flat_map(|y| &screen[row(y)]).copied().collect());
        for y in 0..ch {
            for x in 0..cw {
                let src = &f.buffer[(y * fw + x) * 4..][..4];
                if src[3] != 0 {
                    screen[((fy + y) * w + fx + x) * 4..][..4].copy_from_slice(src);
                }
            }
        }
        for r in [restored.take(), rect].into_iter().flatten() {
            to_out(&screen, &mut out, w, r);
        }
        store.push(&out, t);
        thumbs.push(thumbnail(&out, w as u32, h as u32, thumb_size));
        match f.dispose {
            gif::DisposalMethod::Background => {
                for y in 0..ch {
                    screen[row(y)].fill(0);
                }
                restored = rect;
            }
            gif::DisposalMethod::Previous => {
                if let Some(saved) = saved {
                    for (y, s) in saved.chunks_exact(cw * 4).enumerate() {
                        screen[row(y)].copy_from_slice(s);
                    }
                }
                restored = rect;
            }
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
    fn gif_with_an_oversized_logical_screen_is_an_error() {
        // 1×1 のフレーム 2 枚で、論理画面だけが大きい。控え (16384) は開ける。
        // 65535×65535 は修正前なら展開で約 17 GB を取りに行くので、表の最後に置く。
        for (w, h, ok) in [
            (16384, 1, true),
            (16385, 1, false),
            (1, 16385, false),
            (65535, 65535, false),
        ] {
            let path = write_gif(
                &format!("big-{w}x{h}.gif"),
                w,
                h,
                vec![
                    (0, 0, 1, 1, vec![0], Keep, 10),
                    (0, 0, 1, 1, vec![1], Keep, 10),
                ],
            );
            let r = load_gif(&path, (2, 2));
            std::fs::remove_file(&path).unwrap();
            assert_eq!(r.is_ok(), ok, "{w}x{h}");
        }
    }

    /// Orientation だけを持つ Exif の APP1 (36 バイト)。`be` は TIFF の `MM` (ビッグエンディアン)。
    fn exif_app1(orientation: u16, be: bool) -> Vec<u8> {
        let u16b = |v: u16| if be { v.to_be_bytes() } else { v.to_le_bytes() };
        let u32b = |v: u32| if be { v.to_be_bytes() } else { v.to_le_bytes() };
        let mut tiff = Vec::new();
        tiff.extend_from_slice(if be { b"MM" } else { b"II" });
        tiff.extend(u16b(42));
        tiff.extend(u32b(8));
        tiff.extend(u16b(1));
        tiff.extend(u16b(0x0112)); // タグ
        tiff.extend(u16b(3)); // SHORT
        tiff.extend(u32b(1)); // 個数
        tiff.extend(u16b(orientation));
        tiff.extend([0, 0]);
        tiff.extend(u32b(0)); // 次の IFD なし
        let len = (2 + 6 + tiff.len()) as u16;
        [&[0xFF, 0xE1][..], &len.to_be_bytes(), b"Exif\0\0", &tiff].concat()
    }

    /// 4 象限 (左上・右上・左下・右下) を塗った w×h の画像を、WIC の `container` のエンコーダで書く。
    /// `meta` は (クエリの道, 値) で、フレームのメタデータに `VT_UI2` で書く。
    fn encode(
        name: &str,
        container: &windows::core::GUID,
        (w, h): (u32, u32),
        quads: [[u8; 4]; 4],
        meta: Option<(&str, u16)>,
    ) -> PathBuf {
        use std::mem::ManuallyDrop;
        use windows::Win32::System::Com::StructuredStorage::*;
        use windows::Win32::System::Variant::VT_UI2;
        let path = temp(name);
        let wic = &crate::ui::gfx().wic;
        let mut bgr = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let q = quads[usize::from(y >= h / 2) * 2 + usize::from(x >= w / 2)];
                bgr.extend_from_slice(&q[..3]);
            }
        }
        unsafe {
            let stream = wic.CreateStream().unwrap();
            stream
                .InitializeFromFilename(&HSTRING::from(path.as_os_str()), 0x4000_0000)
                .unwrap();
            let encoder = wic.CreateEncoder(container, std::ptr::null()).unwrap();
            encoder
                .Initialize(&stream, WICBitmapEncoderNoCache)
                .unwrap();
            let mut frame = None;
            encoder
                .CreateNewFrame(&mut frame, std::ptr::null_mut())
                .unwrap();
            let frame = frame.unwrap();
            frame.Initialize(None).unwrap();
            frame.SetSize(w, h).unwrap();
            let mut format = GUID_WICPixelFormat24bppBGR;
            frame.SetPixelFormat(&mut format).unwrap();
            assert_eq!(format, GUID_WICPixelFormat24bppBGR);
            frame.WritePixels(h, w * 3, &bgr).unwrap();
            if let Some((query, value)) = meta {
                let v = PROPVARIANT {
                    Anonymous: PROPVARIANT_0 {
                        Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                            vt: VT_UI2,
                            Anonymous: PROPVARIANT_0_0_0 { uiVal: value },
                            ..Default::default()
                        }),
                    },
                };
                let writer = frame.GetMetadataQueryWriter().unwrap();
                writer.SetMetadataByName(&HSTRING::from(query), &v).unwrap();
            }
            frame.Commit().unwrap();
            encoder.Commit().unwrap();
        }
        path
    }

    /// 4 象限を塗った w×h の JPEG を WIC で書き、SOI の直後に `app1` を差し込む。
    fn write_jpeg(name: &str, w: u32, h: u32, quads: [[u8; 4]; 4], app1: &[u8]) -> PathBuf {
        let path = encode(name, &GUID_ContainerFormatJpeg, (w, h), quads, None);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes[..2], [0xFF, 0xD8]);
        std::fs::write(&path, [&bytes[..2], app1, &bytes[2..]].concat()).unwrap();
        path
    }

    #[test]
    fn jpeg_exif_orientation_is_applied() {
        com();
        // 64×32 を 32×16 の象限に塗る (JPEG の 16×16 のブロックに揃い、色が混ざらない)。
        let quads = [R, G, B, W];
        // (Orientation, 直した大きさ, 直した左上・右上・左下・右下)。期待値は EXIF の定義から手で書いた。
        // Exif の無い JPEG は Orientation 1 と同じ。
        let none = (Vec::new(), (64, 32), [R, G, B, W]);
        let rows = [
            none,
            (exif_app1(1, false), (64, 32), [R, G, B, W]),
            (exif_app1(2, false), (64, 32), [G, R, W, B]),
            (exif_app1(3, false), (64, 32), [W, B, G, R]),
            (exif_app1(4, false), (64, 32), [B, W, R, G]),
            (exif_app1(5, false), (32, 64), [R, B, G, W]),
            (exif_app1(6, false), (32, 64), [B, R, W, G]),
            (exif_app1(7, false), (32, 64), [W, G, B, R]),
            (exif_app1(8, false), (32, 64), [G, W, R, B]),
            (exif_app1(6, true), (32, 64), [B, R, W, G]),
        ];
        for (o, (app1, size, want)) in rows.into_iter().enumerate() {
            let path = write_jpeg(&format!("orient-{o}.jpg"), 64, 32, quads, &app1);
            let img = load_image(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            assert_quads(&img, size, want, &format!("row {o}"));
        }
    }

    /// 大きさと、各象限の中心の色。JPEG の誤差を 40 まで許す。
    fn assert_quads(img: &Image, size: (u32, u32), want: [[u8; 4]; 4], label: &str) {
        assert_eq!((img.w, img.h), size, "{label}");
        let (w, h) = (img.w as usize, img.h as usize);
        for (i, want) in want.iter().enumerate() {
            let (x, y) = ((i % 2 * 2 + 1) * w / 4, (i / 2 * 2 + 1) * h / 4);
            let got = &img.bgra[(y * w + x) * 4..][..4];
            assert!(
                got.iter().zip(want).all(|(a, b)| a.abs_diff(*b) <= 40),
                "{label} quadrant {i}: {got:?} vs {want:?}"
            );
        }
    }

    #[test]
    fn tiff_exif_orientation_is_applied() {
        com();
        // TIFF の向きは IFD0 に直に在る (JPEG の APP1 を通らない)。期待値は JPEG の行 6 と同じ。
        let path = encode(
            "orient-6.tif",
            &GUID_ContainerFormatTiff,
            (64, 32),
            [R, G, B, W],
            Some(("/ifd/{ushort=274}", 6)),
        );
        let img = load_image(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_quads(&img, (32, 64), [B, R, W, G], "tiff 6");
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
