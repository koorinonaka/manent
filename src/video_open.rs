//! 動画ファイルを開く窓 (DS `VideoOpen`)。録画の上限 (`record_limit_secs`) より長い動画は範囲の面で
//! 開く区間を選び、確定したら同じ窓を進捗の面に替える。上限以下の動画は進捗の面だけを、読み込みが
//! 400 ms を超えたら出す。読み終えたら窓を閉じてから GIF ピンを出す (`crate::video_done`)。
//! Media Foundation はスレッドで呼ぶ (`open` は `WM_COPYDATA` の中からも呼ばれ、そこでは COM を呼べない)。
//! スレッドの結果は `Shared` に置き、窓がタイマーで拾う。窓を閉じた後に届いた結果は `Arc` と一緒に捨てられる。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_ALIASED;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SPACE, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};

use crate::record::Recorded;
use crate::timeline::clock;
use crate::ui::widgets::{self, Acrylic, Capture};
use crate::ui::{self, Style, Text, draw_text, fill_rect, fill_round, px, rectf};
use crate::video;

const CLASS: PCWSTR = w!("OdileVideoOpen");
/// スレッドの結果と進捗を拾う。
const TIMER_POLL: usize = 1;
const POLL_MS: u32 = 30;
/// 進捗の面だけを出すとき、読み込みがこれを超えたら出す (`ExportStatus` の進捗と同じ時計)。
const SHOW_AFTER: Duration = Duration::from_millis(400);

// ---- 区間の計算 (秒、1 秒刻み) ----

/// 開く区間 (秒)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Seg {
    pub start: u32,
    pub len: u32,
}

impl Seg {
    fn end(self) -> u32 {
        self.start + self.len
    }
}

/// 既定の区間: 先頭から上限秒 (動画より長くしない、最短 1 秒)。
fn initial(total: f64, limit: u32) -> Seg {
    Seg {
        start: 0,
        len: limit.min(total.floor() as u32).max(1),
    }
}

/// 開始の最大: 区間の終わりを動画の長さに収める。端数の尾 (3:24.5 の 0.5 秒) は 1 秒刻みでは選べない。
fn max_start(len: u32, total: f64) -> u32 {
    (total - f64::from(len)).max(0.0).floor() as u32
}

/// 長さを保って開始を `to` 秒へ (本体のドラッグ・← →)。1 秒刻みに丸め、動画の外へ出さない。
fn moved(seg: Seg, to: f64, total: f64) -> Seg {
    let start = to.round().clamp(0.0, f64::from(max_start(seg.len, total))) as u32;
    Seg { start, ..seg }
}

/// 区間の中央を `at` 秒へ寄せる (溝の区間の外のクリック)。
fn centered(seg: Seg, at: f64, total: f64) -> Seg {
    moved(seg, at - f64::from(seg.len) / 2.0, total)
}

/// 左のつまみを `to` 秒へ。右の端は止める。長さは 1 ..= `limit`。
fn left_to(seg: Seg, to: f64, limit: u32) -> Seg {
    let end = seg.end();
    let start = (to.round().max(0.0) as u32).clamp(end.saturating_sub(limit), end - 1);
    Seg {
        start,
        len: end - start,
    }
}

/// 右のつまみを `to` 秒へ。左の端は止める。長さは 1 ..= `limit`、終わりは動画の中。
fn right_to(seg: Seg, to: f64, total: f64, limit: u32) -> Seg {
    let lo = seg.start + 1;
    let hi = (seg.start + limit).min(total.floor() as u32).max(lo);
    let end = (to.round().max(0.0) as u32).clamp(lo, hi);
    Seg {
        start: seg.start,
        len: end - seg.start,
    }
}

// ---- スレッドとの受け渡し ----

type Decoded = Result<Option<(Recorded, (u32, u32))>>;
type Frame = Result<(u32, u32, Vec<u8>)>;

#[derive(Default)]
struct Shared {
    /// 立てるとデコードを止める (`video::decode` が `Ok(None)` を返す)。
    stop: AtomicBool,
    /// 読んだ割合 (f32 のビット)。
    progress: AtomicU32,
    total: Mutex<Option<Result<Duration>>>,
    /// (区間の開始の秒, 1 枚)。
    preview: Mutex<Option<(u32, Frame)>>,
    done: Mutex<Option<Decoded>>,
}

fn take<T>(m: &Mutex<Option<T>>) -> Option<T> {
    m.lock().ok().and_then(|mut v| v.take())
}

fn put<T>(m: &Mutex<Option<T>>, v: T) {
    if let Ok(mut slot) = m.lock() {
        *slot = Some(v);
    }
}

// ---- 窓 ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Face {
    Range,
    Progress,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    Body,
    Left,
    Right,
    /// 溝の区間の外。
    Groove,
    Btn(Btn),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Btn {
    Cancel,
    Open,
    Abort,
}

#[derive(Clone, Copy, Debug)]
enum Drag {
    /// 押した位置の秒と、押したときの区間。
    Body(f64, Seg),
    /// 押した位置と端の差 (秒) と、押したときの区間。
    Left(f64, Seg),
    Right(f64, Seg),
}

struct Vo {
    win: Acrylic,
    s: f32,
    path: PathBuf,
    /// 開いたときのマウスの位置とその作業領域 (窓とピンの置き場)。
    pt: POINT,
    work: RECT,
    limit: u32,
    /// 動画の長さ (秒)。長さを読むまでは 0。
    total: f64,
    /// 出している面。`None` はまだ出していない (長さか 400 ms を待っている)。
    face: Option<Face>,
    seg: Seg,
    /// 読んでいる区間 (進捗の面の状態の行)。
    span: (Duration, Duration),
    drag: Option<Drag>,
    hover: Option<Hit>,
    pressed: Option<Btn>,
    /// Tab で動かすボタンのフォーカス (`buttons` の添字)。
    focus: Option<usize>,
    /// 今のプレビュー (幅, 高さ, BGRA)。新しい 1 枚が届くまで前の 1 枚を残す。
    preview: Option<(u32, u32, Vec<u8>)>,
    /// プレビューを読むスレッドが走っている開始の秒。
    preview_busy: Option<u32>,
    /// デコードを始めた時刻。
    t0: Option<Instant>,
    /// 最後に描いた進捗 (1/1000)。
    drawn: u32,
    shared: Arc<Shared>,
}

thread_local! {
    static VOS: RefCell<Vec<Vo>> = const { RefCell::new(Vec::new()) };
}

/// `hwnd` の窓の状態に `f` を当てる。使用中 (描画の中から来たメッセージ) なら `None`。
fn with<R>(hwnd: HWND, f: impl FnOnce(&mut Vo) -> R) -> Option<R> {
    VOS.with(|v| {
        let mut v = v.try_borrow_mut().ok()?;
        v.iter_mut().find(|vo| vo.win.hwnd == hwnd).map(f)
    })
}

/// 動画 `path` を開く。`pt` はマウスの位置、`work` はその作業領域。すぐ戻る。
pub fn open(path: &Path, pt: POINT, work: RECT) {
    let settings = crate::settings();
    let limit = settings.record_limit_secs.max(1);
    widgets::register_class(CLASS, proc);
    let win = match Acrylic::new(CLASS, false, Capture::Setting) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("video_open: window failed: {e}");
            crate::video_done(path, pt, work, Err(e));
            return;
        }
    };
    let hwnd = win.hwnd;
    unsafe {
        // 前面を取って Enter・Esc・← → を受ける (`Acrylic::new` は NOACTIVATE を付ける)。
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !(WS_EX_NOACTIVATE.0 as isize));
    }
    let shared = Arc::new(Shared::default());
    let s = ui::monitor_at(pt).dpi.max(96) as f32 / 96.0;
    VOS.with(|v| {
        v.borrow_mut().push(Vo {
            win,
            s,
            path: path.to_path_buf(),
            pt,
            work,
            limit,
            total: 0.0,
            face: None,
            seg: Seg { start: 0, len: 1 },
            span: (Duration::ZERO, Duration::ZERO),
            drag: None,
            hover: None,
            pressed: None,
            focus: None,
            preview: None,
            preview_busy: None,
            t0: Some(Instant::now()),
            drawn: u32::MAX,
            shared: shared.clone(),
        })
    });
    unsafe { SetTimer(Some(hwnd), TIMER_POLL, POLL_MS, None) };
    // 長さを読む。上限以下ならそのまま全体を読む (窓は 400 ms を超えたら出す)。
    let path = path.to_path_buf();
    let job = decode_job(&path, pt, work);
    std::thread::spawn(move || {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let total = video::duration(&path);
        let whole = match &total {
            Ok(d) if *d <= Duration::from_secs(u64::from(limit)) => Some(*d),
            _ => None,
        };
        put(&shared.total, total);
        if let Some(d) = whole {
            job(&shared, Duration::ZERO, d);
        }
    });
}

/// 区間を読むスレッドの中身。大きさ・fps・サムネイルは録画と同じ決め方 (作業領域に収める)。
fn decode_job(
    path: &Path,
    pt: POINT,
    work: RECT,
) -> impl Fn(&Shared, Duration, Duration) + Send + use<> {
    let settings = crate::settings();
    let fps = settings.record_fps.clamp(1, 120);
    let s = ui::monitor_at(pt).dpi.max(96) as f32 / 96.0;
    let thumb = (px(56.0, s).max(1) as u32, px(32.0, s).max(1) as u32);
    let max = (
        (work.right - work.left).max(2) as u32,
        (work.bottom - work.top).max(2) as u32,
    );
    let path = path.to_path_buf();
    move |shared: &Shared, start: Duration, len: Duration| {
        let r = video::decode(&path, start, len, max, fps, thumb, &shared.stop, &mut |v| {
            shared.progress.store(v.to_bits(), Ordering::Relaxed)
        });
        let tag = match &r {
            Ok(Some(_)) => "ok",
            Ok(None) => "stopped",
            Err(_) => "error",
        };
        eprintln!("video_open: decode thread ended ({tag})");
        put(&shared.done, r);
    }
}

/// 窓の外形の大きさ (物理 px)。
fn size(face: Face, s: f32) -> (i32, i32) {
    let h = match face {
        Face::Range => 462.0,
        Face::Progress => 132.0,
    };
    (px(512.0, s), px(h, s))
}

/// 作業領域の中央に面を出す。
fn show_centered(vo: &mut Vo, face: Face) {
    vo.face = Some(face);
    let (w, h) = size(face, vo.s);
    let at = POINT {
        x: vo.work.left + (vo.work.right - vo.work.left - w) / 2,
        y: vo.work.top + (vo.work.bottom - vo.work.top - h) / 2,
    };
    vo.win.show_at(ui::rect_at(at, w, h));
    crate::force_foreground(vo.win.hwnd);
    paint(vo);
}

/// 区間の開始のプレビューを読む。走っているスレッドがあれば、それが終わってから読み直す (`tick`)。
fn want_preview(vo: &mut Vo) {
    if vo.preview_busy.is_some() {
        return;
    }
    let at = vo.seg.start;
    vo.preview_busy = Some(at);
    let (pw, ph) = (px(480.0, vo.s) as u32, px(270.0, vo.s) as u32);
    let (path, shared) = (vo.path.clone(), vo.shared.clone());
    std::thread::spawn(move || {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let t = Instant::now();
        // 縦横比を保って収め、余りを黒で塗った 480 × 270 にする。
        let r = video::frame_at(&path, Duration::from_secs(u64::from(at)), (pw, ph)).map(
            |(w, h, f)| {
                let mut out = vec![0u8; pw as usize * ph as usize * 4];
                for px in out.chunks_exact_mut(4) {
                    px[3] = 255;
                }
                let (ox, oy) = (
                    ((pw - w.min(pw)) / 2) as usize,
                    ((ph - h.min(ph)) / 2) as usize,
                );
                let row = w.min(pw) as usize * 4;
                for y in 0..h.min(ph) as usize {
                    out[((oy + y) * pw as usize + ox) * 4..][..row]
                        .copy_from_slice(&f[y * w as usize * 4..][..row]);
                }
                (pw, ph, out)
            },
        );
        eprintln!(
            "video_open: preview at {at} s in {} ms ({})",
            t.elapsed().as_millis(),
            if r.is_ok() { "ok" } else { "error" }
        );
        put(&shared.preview, (at, r));
    });
}

/// 区間を確定し、同じ窓を進捗の面に替える (上端と左右を保って高さを縮める)。
fn confirm(vo: &mut Vo) {
    let seg = vo.seg;
    eprintln!(
        "video_open: confirm start {} s, len {} s",
        seg.start, seg.len
    );
    let start = Duration::from_secs(u64::from(seg.start));
    let len = Duration::from_secs(u64::from(seg.len));
    vo.span = (start, start + len);
    vo.face = Some(Face::Progress);
    vo.drag = None;
    vo.hover = None;
    vo.pressed = None;
    vo.focus = None;
    let (w, h) = size(Face::Progress, vo.s);
    let mut r = RECT::default();
    let _ = unsafe { GetWindowRect(vo.win.hwnd, &mut r) };
    vo.win.show_at(ui::rect_at(
        POINT {
            x: r.left,
            y: r.top,
        },
        w,
        h,
    ));
    vo.t0 = Some(Instant::now());
    let job = decode_job(&vo.path, vo.pt, vo.work);
    let shared = vo.shared.clone();
    std::thread::spawn(move || {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        job(&shared, start, len);
    });
    paint(vo);
}

/// 窓を閉じてから結果を渡す (ピンは窓が消えてから出す)。`Ok(None)` はキャンセル・中止。
fn finish(hwnd: HWND, result: Decoded) {
    let vo = VOS.with(|v| {
        let mut v = v.borrow_mut();
        let i = v.iter().position(|vo| vo.win.hwnd == hwnd)?;
        Some(v.remove(i))
    });
    let Some(vo) = vo else {
        return;
    };
    // 読んでいるスレッドを止める。結果はスレッドが捨てる (`Arc` の最後の持ち主)。
    vo.shared.stop.store(true, Ordering::Relaxed);
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_POLL);
        let _ = DestroyWindow(hwnd);
    }
    let tag = match &result {
        Ok(Some(_)) => "pin",
        Ok(None) => "cancel",
        Err(_) => "error",
    };
    eprintln!("video_open: closed ({tag})");
    crate::video_done(&vo.path, vo.pt, vo.work, result);
}

/// スレッドの結果と進捗を拾う。
fn tick(hwnd: HWND) {
    let mut end = None;
    with(hwnd, |vo| {
        if vo.total == 0.0 {
            match take(&vo.shared.total) {
                None => {}
                Some(Err(e)) => {
                    end = Some(Err(e));
                    return;
                }
                Some(Ok(d)) => {
                    vo.total = d.as_secs_f64().max(1e-3);
                    eprintln!(
                        "video_open: duration {:.2} s, limit {} s",
                        vo.total, vo.limit
                    );
                    if d > Duration::from_secs(u64::from(vo.limit)) {
                        vo.seg = initial(vo.total, vo.limit);
                        vo.t0 = None;
                        show_centered(vo, Face::Range);
                        eprintln!("video_open: range face");
                        want_preview(vo);
                    } else {
                        vo.span = (Duration::ZERO, d);
                    }
                }
            }
        }
        if let Some((at, frame)) = take(&vo.shared.preview) {
            vo.preview_busy = None;
            match frame {
                Ok(f) => vo.preview = Some(f),
                Err(e) => eprintln!("video_open: preview failed: {e}"),
            }
            if vo.face == Some(Face::Range) {
                if at != vo.seg.start && vo.drag.is_none() {
                    want_preview(vo);
                }
                paint(vo);
            }
        }
        let Some(t0) = vo.t0 else {
            return;
        };
        if let Some(r) = take(&vo.shared.done) {
            end = Some(r);
            return;
        }
        if vo.face.is_none() && vo.total > 0.0 && t0.elapsed() >= SHOW_AFTER {
            show_centered(vo, Face::Progress);
            eprintln!(
                "video_open: progress face after {} ms",
                t0.elapsed().as_millis()
            );
        }
        let p = (f32::from_bits(vo.shared.progress.load(Ordering::Relaxed)) * 1000.0) as u32;
        if vo.face == Some(Face::Progress) && p != vo.drawn {
            paint(vo);
        }
    });
    if let Some(r) = end {
        finish(hwnd, r);
    }
}

// ---- 置き方 (物理 px、窓の外形の左上から) ----

struct Geo {
    /// 溝の左端と幅。
    left: f32,
    w: f32,
    /// シークバーの段の上端。
    stage: f32,
    /// 区間の窓 (描く外形)。
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
}

fn geo(vo: &Vo) -> Geo {
    let p = |v: f32| px(v, vo.s) as f32;
    let (left, w, stage) = (p(16.0), p(480.0), p(346.0));
    let total = vo.total.max(1e-3);
    // 描く幅は最小 16。溝の外へは出さない (当たりも描いた外形で取る)。
    let sw = (w * vo.seg.len as f32 / total as f32).max(p(16.0)).min(w);
    let x0 = (left + w * vo.seg.start as f32 / total as f32).min(left + w - sw);
    Geo {
        left,
        w,
        stage,
        x0,
        x1: x0 + sw,
        y0: stage + p(4.0),
        y1: stage + p(28.0),
    }
}

/// ボタン: 右端を外形の右から 16 に揃え、間は 8。範囲の面は上から 414、進捗の面は 84。
fn buttons(vo: &Vo) -> Vec<(Btn, D2D_RECT_F)> {
    let t = crate::strings();
    let s = vo.s;
    let p = |v: f32| px(v, s) as f32;
    let (list, top): (&[(Btn, &str)], f32) = match vo.face {
        Some(Face::Range) => (
            &[
                (Btn::Cancel, t.video_open_cancel),
                (Btn::Open, t.video_open_open),
            ],
            p(414.0),
        ),
        Some(Face::Progress) => (&[(Btn::Abort, t.video_open_abort)], p(84.0)),
        None => (&[], 0.0),
    };
    let mut right = vo.win.surface.w as f32 - p(16.0);
    let mut out: Vec<_> = list
        .iter()
        .rev()
        .map(|&(b, label)| {
            let w = widgets::text_button_width(label, s) as f32;
            let r = rectf(right - w, top, right, top + p(32.0));
            right -= w + p(8.0);
            (b, r)
        })
        .collect();
    out.reverse();
    out
}

fn inside(r: &D2D_RECT_F, x: f32, y: f32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn hit(vo: &Vo, x: f32, y: f32) -> Option<Hit> {
    if let Some((b, _)) = buttons(vo).into_iter().find(|(_, r)| inside(r, x, y)) {
        return Some(Hit::Btn(b));
    }
    if vo.face != Some(Face::Range) {
        return None;
    }
    let g = geo(vo);
    let p = |v: f32| px(v, vo.s) as f32;
    let (hw, out) = (p(6.0), p(4.0));
    if y >= g.y0 && y < g.y1 {
        if x >= g.x0 - out && x < g.x0 + hw + out {
            return Some(Hit::Left);
        }
        if x >= g.x1 - hw - out && x < g.x1 + out {
            return Some(Hit::Right);
        }
        if x >= g.x0 && x < g.x1 {
            return Some(Hit::Body);
        }
    }
    let stage = rectf(g.left, g.stage, g.left + g.w, g.stage + p(32.0));
    inside(&stage, x, y).then_some(Hit::Groove)
}

/// 窓の x (物理 px) を動画の秒へ。
fn secs_at(vo: &Vo, x: f32) -> f64 {
    let g = geo(vo);
    f64::from((x - g.left) / g.w) * vo.total
}

// ---- 描画 ----

/// `max` に入らなければ末尾を … で切る。
fn fit_text(s_: &str, style: Style, s: f32, max: f32) -> Text {
    let t = ui::text(s_, style, s);
    if t.w <= max {
        return t;
    }
    let chars: Vec<char> = s_.chars().collect();
    let cut = |n: usize| chars[..n].iter().collect::<String>() + "…";
    // 収まる最長の n を二分探索で探す。
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if ui::text(&cut(mid), style, s).w <= max {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    ui::text(&cut(lo), style, s)
}

fn span_text(a: Duration, b: Duration) -> String {
    format!("{} – {}", clock(a), clock(b))
}

fn paint(vo: &mut Vo) {
    let Some(face) = vo.face else {
        return;
    };
    let pal = ui::palette();
    let t = crate::strings();
    let s = vo.s;
    let p = |v: f32| px(v, s) as f32;
    let (left, cw) = (p(16.0), p(480.0));
    let right = left + cw;
    let name = vo.path.file_name().unwrap_or(vo.path.as_os_str());
    let name = fit_text(&name.to_string_lossy(), Style::BodyStrong, s, cw);
    let frac = f32::from_bits(vo.shared.progress.load(Ordering::Relaxed)).clamp(0.0, 1.0);
    vo.drawn = (frac * 1000.0) as u32;
    let g = geo(vo);
    let btns = buttons(vo);
    let vo = &*vo;
    vo.win.paint(pal, |ctx| unsafe {
        draw_text(ctx, &name, left, p(16.0), pal.text_primary);
        match face {
            Face::Range => {
                let limit = clock(Duration::from_secs(u64::from(vo.limit)));
                let hint = fit_text(
                    &t.video_open_hint.replace("{limit}", &limit),
                    Style::Caption,
                    s,
                    cw,
                );
                draw_text(ctx, &hint, left, p(36.0), pal.text_secondary);
                // プレビュー: 480 × 270、角丸 4。届くまでは黒。
                let r = rectf(left, p(64.0), right, p(334.0));
                let bmp = vo
                    .preview
                    .as_ref()
                    .and_then(|(w, h, f)| ui::bitmap_from_bgra(ctx, *w, *h, f).ok());
                match bmp {
                    Some(b) => widgets::timeline::fill_bitmap_round(ctx, &b, r, p(4.0)),
                    None => fill_round(ctx, r, p(4.0), ui::hex(0x000000)),
                }
                // 溝。
                let ty = g.stage + p(14.0);
                fill_round(ctx, rectf(left, ty, right, ty + p(4.0)), p(2.0), pal.track);
                // 区間の窓: 地、上下の線 2、左右のつまみ 6 × 24 (外側の角だけ丸い)。
                let body_drag = matches!(vo.drag, Some(Drag::Body(..)));
                let wash = if body_drag {
                    pal.fill_pressed
                } else if vo.hover == Some(Hit::Body) && vo.drag.is_none() {
                    pal.fill_hover
                } else {
                    pal.select_wash
                };
                let seg_r = rectf(g.x0, g.y0, g.x1, g.y1);
                fill_round(ctx, seg_r, p(4.0), wash);
                let (hw, line) = (p(6.0), ui::stroke(2.0, s) as f32);
                fill_rect(
                    ctx,
                    rectf(g.x0 + hw, g.y0, g.x1 - hw, g.y0 + line),
                    pal.on_fill,
                );
                fill_rect(
                    ctx,
                    rectf(g.x0 + hw, g.y1 - line, g.x1 - hw, g.y1),
                    pal.on_fill,
                );
                for (side, l) in [(Hit::Left, g.x0), (Hit::Right, g.x1 - hw)] {
                    let dragging = matches!(
                        (side, vo.drag),
                        (Hit::Left, Some(Drag::Left(..))) | (Hit::Right, Some(Drag::Right(..)))
                    );
                    let color = if dragging {
                        pal.on_fill_pressed
                    } else if vo.hover == Some(side) && vo.drag.is_none() {
                        pal.on_fill_hover
                    } else {
                        pal.on_fill
                    };
                    ctx.PushAxisAlignedClip(
                        &rectf(l, g.y0, l + hw, g.y1),
                        D2D1_ANTIALIAS_MODE_ALIASED,
                    );
                    fill_round(ctx, seg_r, p(4.0), color);
                    ctx.PopAxisAlignedClip();
                    let (mw, mh) = (p(2.0), p(10.0));
                    let mx = l + ((hw - mw) / 2.0).round();
                    let my = g.y0 + ((g.y1 - g.y0 - mh) / 2.0).round();
                    fill_round(ctx, rectf(mx, my, mx + mw, my + mh), p(1.0), pal.on_content);
                }
                // 時刻の行: 左に区間、右に長さと全体。
                let a = Duration::from_secs(u64::from(vo.seg.start));
                let b = Duration::from_secs(u64::from(vo.seg.end()));
                let span = ui::text(&span_text(a, b), Style::Num, s);
                draw_text(ctx, &span, left, p(382.0), pal.text_primary);
                let len = t
                    .video_open_length
                    .replace("{len}", &clock(b - a))
                    .replace("{total}", &clock(Duration::from_secs_f64(vo.total)));
                // `num` は `caption` を tabular figures で組んだもの (同じ大きさ・太さ)。
                let len = ui::text(&len, Style::Num, s);
                draw_text(ctx, &len, right - len.w, p(382.0), pal.text_secondary);
            }
            Face::Progress => {
                let status = t
                    .video_open_loading
                    .replace("{start}", &clock(vo.span.0))
                    .replace("{end}", &clock(vo.span.1));
                let pct = ui::text(
                    &format!("{}%", (frac * 100.0).floor() as u32),
                    Style::NumStrong,
                    s,
                );
                let status = fit_text(&status, Style::Caption, s, cw - pct.w - p(8.0));
                draw_text(ctx, &status, left, p(36.0), pal.text_secondary);
                draw_text(ctx, &pct, right - pct.w, p(36.0), pal.text_primary);
                let by = p(64.0);
                fill_round(ctx, rectf(left, by, right, by + p(4.0)), p(2.0), pal.track);
                if frac > 0.0 {
                    let fx = left + (cw * frac).round().max(p(4.0));
                    fill_round(ctx, rectf(left, by, fx, by + p(4.0)), p(2.0), pal.on_fill);
                }
            }
        }
        let t = crate::strings();
        for (i, (b, r)) in btns.iter().enumerate() {
            let label = match b {
                Btn::Cancel => t.video_open_cancel,
                Btn::Open => t.video_open_open,
                Btn::Abort => t.video_open_abort,
            };
            let hot = if vo.hover == Some(Hit::Btn(*b)) {
                1.0
            } else {
                0.0
            };
            widgets::text_button(ctx, *r, label, hot, pal, s);
            if vo.focus == Some(i) {
                widgets::focus_ring(ctx, *r, p(4.0), pal, s);
            }
        }
    });
}

// ---- 入力 ----

/// ボタンを押した。
fn press(hwnd: HWND, b: Btn) {
    match b {
        Btn::Cancel | Btn::Abort => finish(hwnd, Ok(None)),
        Btn::Open => {
            with(hwnd, confirm);
        }
    }
}

/// Enter: フォーカスのあるボタン、無ければ範囲の面の「開く」。
fn enter(hwnd: HWND) {
    let b = with(hwnd, |vo| {
        let btns = buttons(vo);
        match (vo.focus.and_then(|i| btns.get(i)), vo.face) {
            (Some((b, _)), _) => Some(*b),
            (None, Some(Face::Range)) => Some(Btn::Open),
            _ => None,
        }
    })
    .flatten();
    if let Some(b) = b {
        press(hwnd, b);
    }
}

fn mouse_move(vo: &mut Vo, x: f32, y: f32) {
    let at = secs_at(vo, x);
    let next = match vo.drag {
        Some(Drag::Body(grab, seg0)) => {
            Some(moved(seg0, f64::from(seg0.start) + at - grab, vo.total))
        }
        Some(Drag::Left(off, seg0)) => Some(left_to(seg0, at - off, vo.limit)),
        Some(Drag::Right(off, seg0)) => Some(right_to(seg0, at - off, vo.total, vo.limit)),
        None => None,
    };
    let hover = hit(vo, x, y);
    let changed = next.is_some_and(|n| n != vo.seg) || hover != vo.hover;
    if let Some(n) = next {
        vo.seg = n;
    }
    vo.hover = hover;
    if changed {
        paint(vo);
    }
}

fn mouse_down(hwnd: HWND, vo: &mut Vo, x: f32, y: f32) {
    let at = secs_at(vo, x);
    let seg = vo.seg;
    match hit(vo, x, y) {
        Some(Hit::Btn(b)) => vo.pressed = Some(b),
        Some(Hit::Body) => vo.drag = Some(Drag::Body(at, seg)),
        Some(Hit::Left) => vo.drag = Some(Drag::Left(at - f64::from(seg.start), seg)),
        Some(Hit::Right) => vo.drag = Some(Drag::Right(at - f64::from(seg.end()), seg)),
        Some(Hit::Groove) => {
            vo.seg = centered(seg, at, vo.total);
            want_preview(vo);
        }
        None => {}
    }
    if vo.drag.is_some() {
        unsafe { SetCapture(hwnd) };
    }
    paint(vo);
}

/// 戻り値は押して離したボタン。
fn mouse_up(vo: &mut Vo, x: f32, y: f32) -> Option<Btn> {
    if vo.drag.take().is_some() {
        let _ = unsafe { ReleaseCapture() };
        eprintln!(
            "video_open: drag end start {} s, len {} s",
            vo.seg.start, vo.seg.len
        );
        // 離したときにプレビューを読み直す。
        want_preview(vo);
        paint(vo);
        return None;
    }
    let b = vo.pressed.take()?;
    (hit(vo, x, y) == Some(Hit::Btn(b))).then_some(b)
}

fn key(hwnd: HWND, vk: u16) {
    if vk == VK_ESCAPE.0 {
        finish(hwnd, Ok(None));
    } else if vk == VK_RETURN.0 {
        enter(hwnd);
    } else if vk == VK_SPACE.0 {
        let b = with(hwnd, |vo| {
            vo.focus.and_then(|i| buttons(vo).get(i).map(|b| b.0))
        })
        .flatten();
        if let Some(b) = b {
            press(hwnd, b);
        }
    } else if vk == VK_TAB.0 {
        with(hwnd, |vo| {
            let n = buttons(vo).len();
            if n > 0 {
                vo.focus = Some(vo.focus.map_or(0, |i| (i + 1) % n));
                paint(vo);
            }
        });
    } else if vk == VK_LEFT.0 || vk == VK_RIGHT.0 {
        with(hwnd, |vo| {
            if vo.face != Some(Face::Range) || vo.drag.is_some() {
                return;
            }
            let d = if vk == VK_LEFT.0 { -1.0 } else { 1.0 };
            let next = moved(vo.seg, f64::from(vo.seg.start) + d, vo.total);
            if next != vo.seg {
                vo.seg = next;
                want_preview(vo);
                paint(vo);
            }
        });
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let xy = || {
        let p = ui::lparam_point(lparam);
        (p.x as f32, p.y as f32)
    };
    match msg {
        WM_TIMER if wparam.0 == TIMER_POLL => {
            tick(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy();
            with(hwnd, |vo| mouse_move(vo, x, y));
            ui::track_leave(hwnd);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            with(hwnd, |vo| {
                if vo.hover.take().is_some() {
                    paint(vo);
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = xy();
            with(hwnd, |vo| mouse_down(hwnd, vo, x, y));
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = xy();
            if let Some(b) = with(hwnd, |vo| mouse_up(vo, x, y)).flatten() {
                press(hwnd, b);
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            with(hwnd, |vo| {
                if vo.drag.take().is_some() {
                    want_preview(vo);
                    paint(vo);
                }
            });
            LRESULT(0)
        }
        WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
            let mut c = POINT::default();
            let _ = unsafe { GetCursorPos(&mut c) };
            let _ = unsafe { windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut c) };
            let id = with(hwnd, |vo| match vo.drag {
                Some(Drag::Body(..)) => IDC_SIZEALL,
                Some(_) => IDC_SIZEWE,
                None => match hit(vo, c.x as f32, c.y as f32) {
                    Some(Hit::Body) => IDC_SIZEALL,
                    Some(Hit::Left | Hit::Right) => IDC_SIZEWE,
                    _ => IDC_ARROW,
                },
            })
            .unwrap_or(IDC_ARROW);
            unsafe {
                if let Ok(cur) = LoadCursorW(None, id) {
                    SetCursor(Some(cur));
                }
            }
            LRESULT(1)
        }
        WM_KEYDOWN => {
            key(hwnd, wparam.0 as u16);
            LRESULT(0)
        }
        // Alt+F4: キャンセル・中止と同じ。
        WM_CLOSE => {
            finish(hwnd, Ok(None));
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: u32, len: u32) -> Seg {
        Seg { start, len }
    }

    /// 全体 3:24 (204.4 秒)、上限 60 秒。区間の計算の表 (DS `VideoOpen` の決まり)。
    #[test]
    fn range_math() {
        let (total, limit) = (204.4, 60);
        assert_eq!(initial(total, limit), seg(0, 60));
        // 上限より短い動画は全体 (範囲の面は出ないが、既定の計算は動画の外へ出さない)。
        assert_eq!(initial(42.7, limit), seg(0, 42));
        // (本体のドラッグ先の開始, 結果): 1 秒刻み、動画の外で止まる (終わりは 204 ≤ 204.4)。
        for (to, want) in [
            (71.6, seg(72, 60)),
            (71.4, seg(71, 60)),
            (-5.0, seg(0, 60)),
            (190.0, seg(144, 60)),
        ] {
            assert_eq!(moved(seg(0, 60), to, total), want, "moved to {to}");
        }
        // ← → は 1 秒ずつ、端で止まる。
        assert_eq!(moved(seg(10, 60), 11.0, total), seg(11, 60));
        assert_eq!(moved(seg(0, 60), -1.0, total), seg(0, 60));
        assert_eq!(moved(seg(144, 60), 145.0, total), seg(144, 60));
        // 溝のクリック: 区間の中央を寄せる。端では収まる所。
        assert_eq!(centered(seg(0, 60), 100.0, total), seg(70, 60));
        assert_eq!(centered(seg(70, 60), 3.0, total), seg(0, 60));
        assert_eq!(centered(seg(0, 60), 204.0, total), seg(144, 60));
        // 右のつまみ: 左の端は止まる。縮める・最短 1 秒・上限を超えない・動画の外へ出さない。
        assert_eq!(right_to(seg(72, 60), 114.2, total, limit), seg(72, 42));
        assert_eq!(right_to(seg(72, 60), 10.0, total, limit), seg(72, 1));
        assert_eq!(right_to(seg(72, 42), 200.0, total, limit), seg(72, 60));
        assert_eq!(right_to(seg(170, 20), 300.0, total, limit), seg(170, 34));
        // 左のつまみ: 右の端 (132) は止まる。
        assert_eq!(left_to(seg(72, 60), 100.4, limit), seg(100, 32));
        assert_eq!(left_to(seg(72, 60), 140.0, limit), seg(131, 1));
        assert_eq!(left_to(seg(100, 32), 0.0, limit), seg(72, 60));
        assert_eq!(left_to(seg(10, 30), -3.0, limit), seg(0, 40));
    }
}
