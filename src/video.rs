//! 動画ファイル (mp4/mov/webm/mkv/avi) を Media Foundation で読み、GIF ピンに渡せる `Recorded` にする。
//! 音声は読まない。呼ぶスレッドは COM (MTA) を初期化しておく (`decode` / `duration` は MF の起動と終了を自分で行う)。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::VT_I8;
use windows::core::{Error, GUID, HSTRING, Result};

use crate::clip::FrameStore;
use crate::record::{Recorded, thumbnail};

/// 動画として開く拡張子 (小文字)。中身ではなく拡張子で決める。
pub const EXTENSIONS: [&str; 5] = ["mp4", "mov", "webm", "mkv", "avi"];

pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

const VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
const SOURCE: u32 = MF_SOURCE_READER_MEDIASOURCE.0 as u32;
/// MF の時刻の単位 (100 ns) の 1 秒。
const HNS: f64 = 10_000_000.0;

/// `MFStartup` から `MFShutdown` まで。
struct Mf;
impl Mf {
    fn start() -> Result<Self> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)? };
        Ok(Self)
    }
}
impl Drop for Mf {
    fn drop(&mut self) {
        let _ = unsafe { MFShutdown() };
    }
}

fn fail(e: impl std::fmt::Display) -> Error {
    Error::new(E_FAIL, e.to_string())
}

fn reader(path: &Path) -> Result<IMFSourceReader> {
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.ok_or_else(|| fail("MFCreateAttributes"))?;
        // YUV → RGB32 の変換と縮小を Source Reader に任せる。
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs)
    }
}

fn duration_of(r: &IMFSourceReader) -> Result<Duration> {
    let v = unsafe { r.GetPresentationAttribute(SOURCE, &MF_PD_DURATION)? };
    let hns = unsafe { v.Anonymous.Anonymous.Anonymous.uhVal };
    Ok(Duration::from_nanos(hns.saturating_mul(100)))
}

/// 動画の長さ。
pub fn duration(path: &Path) -> Result<Duration> {
    let _mf = Mf::start()?;
    duration_of(&reader(path)?)
}

fn size_of(t: &IMFMediaType) -> Result<(u32, u32)> {
    let v = unsafe { t.GetUINT64(&MF_MT_FRAME_SIZE)? };
    Ok(((v >> 32) as u32, v as u32))
}

/// `w`×`h` を `max` に収める大きさ (拡大はしない)。縦横は偶数に丸める (最小 2)。
fn fit((w, h): (u32, u32), (mw, mh): (u32, u32)) -> (u32, u32) {
    let k = (mw as f64 / w as f64).min(mh as f64 / h as f64).min(1.0);
    let even = |v: u32| (((f64::from(v) * k / 2.0).round() as u32) * 2).max(2);
    if k >= 1.0 { (w, h) } else { (even(w), even(h)) }
}

/// 縦横比を保って `max` にちょうど収める大きさ (拡大もする)。縦横は偶数に丸める (最小 2)。
fn fit_up((w, h): (u32, u32), (mw, mh): (u32, u32)) -> (u32, u32) {
    let k = (mw as f64 / w as f64).min(mh as f64 / h as f64);
    let even = |v: u32| (((f64::from(v) * k / 2.0).floor() as u32) * 2).max(2);
    (even(w), even(h))
}

/// 映像だけを RGB32 で読む Source Reader。大きさは回転を戻した向きの元の大きさを `size` で決める。
/// 戻り値は (reader, 動画の長さ, 出るフレームの大きさ)。
fn open(
    path: &Path,
    size: impl Fn((u32, u32)) -> (u32, u32),
) -> Result<(IMFSourceReader, Duration, (u32, u32))> {
    let r = reader(path)?;
    let total = duration_of(&r)?;
    unsafe {
        r.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        r.SetStreamSelection(VIDEO, true)?;
    }
    let native = unsafe { r.GetNativeMediaType(VIDEO, 0)? };
    // 縦に撮ったスマホの動画は `MF_MT_VIDEO_ROTATION` (90/270) を持つ。Source Reader は回して出すが、
    // 出力の大きさは頼んだまま (横長に頼むと左右に黒い帯)。縦横を入れ替えて頼む (2026-11-18 実測)。
    let (nw, nh) = size_of(&native)?;
    let rotation = unsafe { native.GetUINT32(&MF_MT_VIDEO_ROTATION) }.unwrap_or(0);
    let upright = if rotation % 180 == 90 {
        (nh, nw)
    } else {
        (nw, nh)
    };
    let (w, h) = size(upright);
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(w) << 32) | u64::from(h))?;
        r.SetCurrentMediaType(VIDEO, None, &t)?;
    }
    let wh = size_of(&unsafe { r.GetCurrentMediaType(VIDEO)? })?;
    Ok((r, total, wh))
}

/// `start` の前のキーフレームへシークする (0 なら何もしない)。
fn seek(r: &IMFSourceReader, start: Duration) -> Result<()> {
    if start > Duration::ZERO {
        let mut pos = PROPVARIANT::default();
        unsafe {
            (*pos.Anonymous.Anonymous).vt = VT_I8;
            (*pos.Anonymous.Anonymous).Anonymous.hVal = (start.as_nanos() / 100) as i64;
            r.SetCurrentPosition(&GUID::zeroed(), &pos)?;
        }
    }
    Ok(())
}

/// 次のサンプル。終わりなら `None`、サンプルの無い知らせ (ギャップなど) は `Some(None)`。
fn next_sample(r: &IMFSourceReader) -> Result<Option<Option<(f64, IMFSample)>>> {
    let (mut flags, mut t, mut sample) = (0u32, 0i64, None);
    unsafe {
        r.ReadSample(
            VIDEO,
            0,
            None,
            Some(&mut flags),
            Some(&mut t),
            Some(&mut sample),
        )?
    };
    if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
        return Ok(None);
    }
    Ok(Some(sample.map(|s| (t as f64 / HNS, s))))
}

/// 時刻 `at` に映っているフレーム 1 枚 (プレビュー)。縦横比を保って `max` にちょうど収める (拡大もする)。
/// 戻り値は (幅, 高さ, BGRA)。
pub fn frame_at(path: &Path, at: Duration, max: (u32, u32)) -> Result<(u32, u32, Vec<u8>)> {
    let _mf = Mf::start()?;
    let (r, _, (w, h)) = open(path, |wh| fit_up(wh, max))?;
    seek(&r, at)?;
    let mut bgra = vec![0u8; w as usize * h as usize * 4];
    let mut got = false;
    // `at` 以前の最後のフレーム。`at` より後しか無ければ最初の 1 枚。
    while let Some(next) = next_sample(&r)? {
        let Some((t, sample)) = next else { continue };
        if got && t > at.as_secs_f64() + 1e-4 {
            break;
        }
        copy_frame(&sample, w, h, &mut bgra)?;
        got = true;
    }
    if !got {
        return Err(fail("動画のフレームが無い"));
    }
    Ok((w, h, bgra))
}

/// 区間 `[start, start+len)` を、`max` (物理 px) に収まる大きさ、`fps` 以下で `Recorded` にする。
/// 戻り値の 2 つめはフレームの大きさ (物理 px)。
/// `ts` は区間の開始からの時刻 (先頭は 0)。`stop` が立てば `Ok(None)`。
/// `progress` は 0..=1 で、読んだサンプルごとに呼ぶ。シークは開始の前のキーフレームに着地するので、
/// 割合は最初に読んだサンプルの時刻から区間の終わりまでのうち読んだ時間 (開始の前を読み飛ばす間も進む)。
#[allow(clippy::too_many_arguments)]
pub fn decode(
    path: &Path,
    start: Duration,
    len: Duration,
    max: (u32, u32),
    fps: u32,
    thumb_size: (u32, u32),
    stop: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<Option<(Recorded, (u32, u32))>> {
    let _mf = Mf::start()?;
    let (r, total, (w, h)) = open(path, |wh| fit(wh, max))?;
    seek(&r, start)?;
    let end = (start + len).min(total);
    let period = 1.0 / f64::from(fps.max(1));
    // 前に採ったフレームから `period` 進んだら採る。時刻の丸め (60 fps の 33.33 ms が 33.3333 ms に届かない)
    // で 1 枚飛ばさないよう、`period` の 1/4 だけ早く採る。
    let slack = period / 4.0;
    let mut next = start.as_secs_f64();
    let mut store = FrameStore::new(w, h, fps.max(1) as usize);
    let (mut ts, mut thumbs) = (Vec::new(), Vec::new());
    let mut bgra = vec![0u8; w as usize * h as usize * 4];
    // 最初に読んだサンプルの時刻 (進捗の起点)。
    let mut from = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let Some(next_one) = next_sample(&r)? else {
            break;
        };
        let Some((at, sample)) = next_one else {
            continue;
        };
        if at >= end.as_secs_f64() {
            break;
        }
        let from = *from.get_or_insert(at);
        progress(((at - from) / (end.as_secs_f64() - from).max(1e-9)).clamp(0.0, 1.0) as f32);
        // シークは前のキーフレームに着地する。区間より前は捨てる。間引くフレームも捨てる。
        if at + slack < next {
            continue;
        }
        while next <= at + slack {
            next += period;
        }
        copy_frame(&sample, w, h, &mut bgra)?;
        let rel = if ts.is_empty() {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((at - start.as_secs_f64()).max(0.0))
        };
        store.push(&bgra, rel);
        thumbs.push(thumbnail(&bgra, w, h, thumb_size));
        ts.push(rel);
    }
    if ts.is_empty() {
        return Err(fail("動画のフレームが無い"));
    }
    store.finish();
    progress(1.0);
    let rec = Recorded {
        store,
        ts,
        end: end.saturating_sub(start),
        thumbs,
        thumb_size,
    };
    Ok(Some((rec, (w, h))))
}

/// サンプルの RGB32 (BGRX) を上から下の BGRA に写す。α は 255。
fn copy_frame(sample: &IMFSample, w: u32, h: u32, out: &mut [u8]) -> Result<()> {
    let row = w as usize * 4;
    unsafe {
        let buf = sample.ConvertToContiguousBuffer()?;
        let (mut p, mut pitch) = (std::ptr::null_mut(), 0i32);
        let b2: Option<IMF2DBuffer> = windows::core::Interface::cast(&buf).ok();
        if let Some(b2) = &b2 {
            b2.Lock2D(&mut p, &mut pitch)?;
        } else {
            buf.Lock(&mut p, None, None)?;
            pitch = row as i32;
        }
        for y in 0..h as usize {
            let src = std::slice::from_raw_parts(p.offset(y as isize * pitch as isize), row);
            out[y * row..][..row].copy_from_slice(src);
        }
        if let Some(b2) = &b2 {
            b2.Unlock2D()?;
        } else {
            buf.Unlock()?;
        }
    }
    for px in out.chunks_exact_mut(4) {
        px[3] = 255;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    /// テスト用の mp4 を SinkWriter (H.264) で書く。フレーム `i` は灰色 `i` (R=G=B=i、255 で頭打ち)。
    /// `rotation` は `MF_MT_VIDEO_ROTATION` (度)。
    pub(crate) fn write_mp4(path: &Path, w: u32, h: u32, fps: u32, frames: u32, rotation: u32) {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _mf = Mf::start().unwrap();
            let _ = std::fs::remove_file(path);
            let sink =
                MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, None).unwrap();
            let out = MFCreateMediaType().unwrap();
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).unwrap();
            out.SetUINT32(&MF_MT_AVG_BITRATE, 4_000_000).unwrap();
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .unwrap();
            out.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(w) << 32) | u64::from(h))
                .unwrap();
            out.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps) << 32) | 1)
                .unwrap();
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)
                .unwrap();
            if rotation != 0 {
                out.SetUINT32(&MF_MT_VIDEO_ROTATION, rotation).unwrap();
            }
            let stream = sink.AddStream(&out).unwrap();
            let inp = MFCreateMediaType().unwrap();
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).unwrap();
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .unwrap();
            inp.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(w) << 32) | u64::from(h))
                .unwrap();
            inp.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps) << 32) | 1)
                .unwrap();
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)
                .unwrap();
            inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, w * 4).unwrap();
            sink.SetInputMediaType(stream, &inp, None).unwrap();
            sink.BeginWriting().unwrap();
            let dur = 10_000_000 / i64::from(fps);
            let bytes = w * h * 4;
            for i in 0..frames {
                let buf = MFCreateMemoryBuffer(bytes).unwrap();
                let mut p = std::ptr::null_mut();
                buf.Lock(&mut p, None, None).unwrap();
                let v = i.min(255) as u8;
                let px = std::slice::from_raw_parts_mut(p, bytes as usize);
                for (j, c) in px.chunks_exact_mut(4).enumerate() {
                    // 上半分は灰色 i、下半分は黒 (向きを確かめる)。
                    let g = if (j as u32 / w) < h / 2 { v } else { 0 };
                    c.copy_from_slice(&[g, g, g, 255]);
                }
                buf.Unlock().unwrap();
                buf.SetCurrentLength(bytes).unwrap();
                let s = MFCreateSample().unwrap();
                s.AddBuffer(&buf).unwrap();
                s.SetSampleTime(i64::from(i) * dur).unwrap();
                s.SetSampleDuration(dur).unwrap();
                sink.WriteSample(stream, &s).unwrap();
            }
            sink.Finalize().unwrap();
        }
    }

    fn fixture(name: &str, w: u32, h: u32, rotation: u32) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("manent-video-{name}.mp4"));
        write_mp4(&path, w, h, 60, 180, rotation);
        path
    }

    fn run(
        path: &Path,
        start: f64,
        len: f64,
        max: (u32, u32),
        stop: bool,
    ) -> Option<(Recorded, (u32, u32))> {
        decode(
            path,
            Duration::from_secs_f64(start),
            Duration::from_secs_f64(len),
            max,
            30,
            (8, 8),
            &AtomicBool::new(stop),
            &mut |_| {},
        )
        .unwrap()
    }

    /// 60 fps・3 秒 (180 枚) を `record_fps` 30 で読むと、1 枚おきの 90 枚・約 33 ms 間隔になる。
    #[test]
    fn thins_to_record_fps() {
        let p = fixture("thin", 640, 360, 0);
        let (rec, size) = run(&p, 0.0, 10.0, (10_000, 10_000), false).unwrap();
        assert_eq!(size, (640, 360));
        assert!(rec.ts.len().abs_diff(90) <= 1, "{} frames", rec.ts.len());
        for d in rec.ts.windows(2).map(|w| w[1] - w[0]) {
            assert!((30..=37).contains(&d.as_millis()), "{d:?}");
        }
        assert_eq!(rec.ts[0], Duration::ZERO);
        assert_eq!(rec.thumbs.len(), rec.ts.len());
        // 向き: 上半分が灰色、下半分が黒。
        let f = rec.store.frame(45).bgra;
        let at = |y: usize| f[(y * 640 + 320) * 4];
        assert!(
            at(10) > 30 && at(350) < 10,
            "top {} bottom {}",
            at(10),
            at(350)
        );
    }

    /// 最大の大きさを渡すと、そこに収まる大きさで読む。
    #[test]
    fn scales_to_fit() {
        let p = fixture("scale", 640, 360, 0);
        let (rec, size) = run(&p, 0.0, 1.0, (320, 180), false).unwrap();
        assert_eq!(size, (320, 180));
        assert_eq!(rec.store.frame(0).bgra.len(), 320 * 180 * 4);
    }

    /// 区間は開始より前のフレームを捨てる (シークは前のキーフレームに着地する)。フレーム i は灰色 i。
    #[test]
    fn segment_starts_at_start() {
        let p = fixture("segment", 640, 360, 0);
        let (rec, _) = run(&p, 1.0, 1.0, (10_000, 10_000), false).unwrap();
        let g = rec.store.frame(0).bgra[(10 * 640 + 320) * 4];
        assert!(g.abs_diff(60) <= 4, "first frame gray {g}");
        assert!(rec.ts.len().abs_diff(30) <= 1, "{} frames", rec.ts.len());
        assert_eq!(rec.end, Duration::from_secs(1));
    }

    /// 区間の開始が 0 でないときも、進捗は読み飛ばす前置きの間から呼ばれ、単調に増えて 1 で終わる。
    /// 採るフレーム (30 fps に間引いた ts) より多く呼ばれる = 捨てたサンプルでも呼ぶ。
    /// 開始 1.5 秒はキーフレーム (1 秒ごと) の間なので、シークは 1.0 秒に着地して 0.5 秒を読み飛ばす。
    #[test]
    fn progress_moves_while_skipping() {
        let p = fixture("progress", 64, 64, 0);
        let mut seen = Vec::new();
        let (rec, _) = decode(
            &p,
            Duration::from_millis(1500),
            Duration::from_secs(1),
            (10_000, 10_000),
            30,
            (8, 8),
            &AtomicBool::new(false),
            &mut |v| seen.push(v),
        )
        .unwrap()
        .unwrap();
        assert!(
            seen.len() > rec.ts.len() + 1,
            "{} calls, {} frames",
            seen.len(),
            rec.ts.len()
        );
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
        assert!(seen.iter().all(|v| (0.0..=1.0).contains(v)), "{seen:?}");
        assert_eq!(seen.last(), Some(&1.0));
        // 最初に採ったフレームより前に呼ばれた分は、開始の前の前置き (0 から上がっていく)。
        assert!(seen[0] < 0.05 && seen[seen.len() / 2] > 0.2, "{seen:?}");
    }

    /// プレビューの 1 枚は、シークの着地 (キーフレーム 1.0 秒 = 灰色 60) ではなく `at` に映るフレーム
    /// (1.5 秒 = フレーム 90)。小さい動画も枠いっぱいに拡大する (640×360 → 480×270)。
    #[test]
    fn frame_at_shows_the_time() {
        let p = fixture("frame-at", 640, 360, 0);
        let (w, h, f) = frame_at(&p, Duration::from_millis(1500), (480, 270)).unwrap();
        assert_eq!((w, h), (480, 270));
        let g = f[(10 * 480 + 240) * 4];
        assert!(g.abs_diff(90) <= 4, "gray {g}");
        let (w, h, _) = frame_at(&p, Duration::ZERO, (960, 1000)).unwrap();
        assert_eq!((w, h), (960, 540));
    }

    /// 中止のフラグが立っていれば何も返さない (立っていない場合は他のテストが Some を見る)。
    #[test]
    fn stop_returns_none() {
        let p = fixture("stop", 64, 64, 0);
        assert!(run(&p, 0.0, 1.0, (10_000, 10_000), true).is_none());
        assert!(run(&p, 0.0, 1.0, (10_000, 10_000), false).is_some());
    }

    /// 縦の動画 (回転 90) は縦長で出る。横長で頼むと左右に黒い帯が付く (2026-11-18 実測)。
    #[test]
    fn rotated_comes_out_upright() {
        let p = fixture("rotate", 640, 360, 90);
        let (rec, size) = run(&p, 0.0, 1.0, (10_000, 10_000), false).unwrap();
        assert_eq!(size, (360, 640));
        let f = rec.store.frame(rec.ts.len() - 1).bgra;
        let at = |x: usize| f[(320 * 360 + x) * 4];
        // 元の上半分 (灰色) が片側に、下半分 (黒) が反対側に来る。帯なら両端とも黒。
        assert!(
            at(2).max(at(357)) > 10 && at(2).min(at(357)) < 5,
            "{} {}",
            at(2),
            at(357)
        );
    }

    #[test]
    fn video_extensions() {
        assert!(is_video(Path::new(r"C:\a\b.MP4")));
        assert!(is_video(Path::new("x.webm")));
        assert!(!is_video(Path::new("x.gif")));
        assert!(!is_video(Path::new("mp4")));
    }
}
