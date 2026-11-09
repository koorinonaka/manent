//! 動画の録画: 選んだ矩形を Windows.Graphics.Capture で取り、`FrameStore` に貯める。
//! 録画は専用スレッド (自分の D3D11 デバイスを持つ。`ui::gfx()` はメインスレッド専用)。
//! 録画枠 (透明な窓に D2D) と録画バー (残り・停止、経過のツールチップ) はメインスレッドの窓。
//! 止めると (同じホットキー・停止のボタン・上限時間) その位置に GIF ピンを出す (`gifpin`)。

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Foundation::Metadata::ApiInformation;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::{DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    ValidateRect,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, Interface, PCWSTR, h, w};

use crate::clip::FrameStore;
use crate::timeline::clock;
use crate::ui::widgets::{self, Acrylic, Capture, Tooltip, place_tooltip};
use crate::ui::{
    self, Style, Surface, draw_text, fill_round, lparam_point, px, rectf, stroke, track_leave,
};

const FRAME_CLASS: PCWSTR = w!("OdileRecFrame");
const BAR_CLASS: PCWSTR = w!("OdileRecBar");
const TIMER_TICK: usize = 1;

/// 録画の結果。`thumbs` はフィルムストリップのサムネイル (BGRA、`thumb_size`)。
pub struct Recorded {
    pub store: FrameStore,
    pub ts: Vec<Duration>,
    /// 録画を止めた時刻 (`ts` と同じ基準)。
    pub end: Duration,
    pub thumbs: Vec<Vec<u8>>,
    pub thumb_size: (u32, u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hot {
    Time,
    Stop,
}

struct Rec {
    /// 録画する矩形 (仮想画面の物理 px)。
    rect: RECT,
    started: Instant,
    limit: Duration,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<Recorded, String>>>,
    frame: HWND,
    _frame_surface: Surface,
    bar: Acrylic,
    tip: Acrylic,
    s: f32,
    work: RECT,
    bar_rect: RECT,
    hover: Option<Hot>,
    pressed: bool,
}

thread_local! {
    static REC: RefCell<Option<Rec>> = const { RefCell::new(None) };
}

pub fn active() -> bool {
    REC.with(|r| r.try_borrow().map(|r| r.is_some()).unwrap_or(true))
}

fn register() {
    thread_local!(static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    if !DONE.with(|d| d.replace(true)) {
        widgets::register_class(FRAME_CLASS, frame_proc);
        widgets::register_class(BAR_CLASS, bar_proc);
    }
}

/// 録画枠の線 (`rec` 2 の 1 本) と、その全幅。白と黒の縁は付けない (赤だけで明暗どちらの地でも見える)。
fn frame_lines(
    s: f32,
) -> [(
    i32,
    windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F,
); 1] {
    [(stroke(2.0, s), ui::REC)]
}

fn frame_width(s: f32) -> i32 {
    frame_lines(s).iter().map(|l| l.0).sum()
}

fn grow(r: RECT, d: i32) -> RECT {
    RECT {
        left: r.left - d,
        top: r.top - d,
        right: r.right + d,
        bottom: r.bottom + d,
    }
}

/// 録画バーの左上: 枠の外形の下端から `gap` 下、右端を外形に揃える。
/// 下に入らなければ上、どちらも入らなければ矩形の内側の右下 (内側へ `gap`)。
fn place_bar(outer: RECT, rect: RECT, w: i32, h: i32, gap: i32, work: RECT) -> POINT {
    let x = outer.right - w;
    let below = outer.bottom + gap;
    if below + h <= work.bottom {
        return POINT { x, y: below };
    }
    let above = outer.top - gap - h;
    if above >= work.top {
        return POINT { x, y: above };
    }
    POINT {
        x: rect.right - gap - w,
        y: rect.bottom - gap - h,
    }
}

/// 録画バーの中の位置 (外形の左上から): 残りの段、残りの時間、停止のボタン、外形の幅。
/// 並び: 余白 4 | 左 8・点の場所 6・gap 6・「残り」・gap 6・時間・右 8 | gap 2 | 停止 32 | 余白 4。
struct BarLayout {
    row: RECT,
    dot_x: f32,
    label_x: f32,
    time: RECT,
    stop: RECT,
    w: i32,
}

fn bar_layout(label_w: f32, time_w: f32, s: f32) -> BarLayout {
    let pad = px(4.0, s);
    let h = px(40.0, s);
    let row_l = pad;
    let dot_x = row_l + px(8.0, s);
    let label_x = dot_x + px(6.0, s) + px(6.0, s);
    let time_l = label_x + label_w.ceil() as i32 + px(6.0, s);
    let time_r = time_l + time_w.ceil() as i32;
    let row_r = time_r + px(8.0, s);
    let stop_l = row_r + px(2.0, s);
    let b = px(32.0, s);
    BarLayout {
        row: RECT {
            left: row_l,
            top: pad,
            right: row_r,
            bottom: h - pad,
        },
        dot_x: dot_x as f32,
        label_x: label_x as f32,
        time: RECT {
            left: time_l,
            top: pad,
            right: time_r,
            bottom: h - pad,
        },
        stop: RECT {
            left: stop_l,
            top: pad,
            right: stop_l + b,
            bottom: pad + b,
        },
        w: stop_l + b + pad,
    }
}

fn layout_now(limit: Duration, s: f32) -> BarLayout {
    let label = ui::text(crate::strings().remaining, Style::Caption, s);
    // 時間の幅は上限の書き方で決める (tabular figures なので桁が同じなら幅も同じ。バーの幅は変わらない)。
    let time = ui::text(&clock(limit), Style::NumStrong, s)
        .w
        .max(ui::text("0:00", Style::NumStrong, s).w);
    bar_layout(label.w, time, s)
}

fn monitor_of(p: POINT) -> (HMONITOR, RECT, RECT, f32) {
    unsafe {
        let m = MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(m, &mut info);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        (m, info.rcMonitor, info.rcWork, dx as f32 / 96.0)
    }
}

/// 枠だけを出した範囲 (録画の前のカウントダウンの間)。`start_prepared` で録画を始め、`cancel` で枠を消す。
pub struct Prepared {
    monitor: HMONITOR,
    mon: RECT,
    /// 選んだ点のモニタに切り詰めた、録画する矩形。
    pub rect: RECT,
    pub work: RECT,
    pub s: f32,
    frame: HWND,
    frame_surface: Surface,
}

/// 範囲が決まった: 録画を始める。
pub fn start(sel: RECT) {
    if let Some(p) = prepare(sel) {
        start_prepared(p);
    }
}

/// 範囲を選んだ点 (左上) のモニタに切り詰め、録画枠だけを出す。録画中・範囲が空・窓が作れないなら `None`。
pub fn prepare(sel: RECT) -> Option<Prepared> {
    if active() {
        return None;
    }
    register();
    // ponytail: 単一モニタのみ。選択開始点 (左上) のモニタに切り詰める。複数モニタ合成が要るなら足す。
    // 窓選択でも、そのモニタの画面から切り出す (窓の上に重なった物も写る)。窓だけを撮るなら CreateForWindow を足す。
    let (monitor, mon, work, s) = monitor_of(POINT {
        x: sel.left,
        y: sel.top,
    });
    let rect = RECT {
        left: sel.left.max(mon.left),
        top: sel.top.max(mon.top),
        right: sel.right.min(mon.right),
        bottom: sel.bottom.min(mon.bottom),
    };
    if rect.right - rect.left < 1 || rect.bottom - rect.top < 1 {
        return None;
    }
    let (frame, frame_surface) = match frame_window(rect, s) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("record: frame window failed: {e}");
            return None;
        }
    };
    Some(Prepared {
        monitor,
        mon,
        rect,
        work,
        s,
        frame,
        frame_surface,
    })
}

/// 録画を始めずに枠を消す (カウントダウンの中止)。
pub fn cancel(p: Prepared) {
    unsafe {
        let _ = DestroyWindow(p.frame);
    }
}

/// 枠を出した範囲で録画を始める (録画のバーもここで出す)。
pub fn start_prepared(p: Prepared) {
    if active() {
        cancel(p);
        return;
    }
    let Prepared {
        monitor,
        mon,
        rect,
        work,
        s,
        frame,
        frame_surface,
    } = p;
    let settings = crate::settings();
    let fps = settings.record_fps.clamp(1, 120);
    let limit = Duration::from_secs(u64::from(settings.record_limit_secs.max(1)));
    let cursor = settings.cursor_in_video;
    let thumb_size = (px(56.0, s).max(1) as u32, px(32.0, s).max(1) as u32);

    let (bar, tip) = match (
        Acrylic::new(BAR_CLASS, false, Capture::Never),
        Acrylic::new(BAR_CLASS, true, Capture::Never),
    ) {
        (Ok(b), Ok(t)) => (b, t),
        _ => {
            eprintln!("record: bar window failed");
            unsafe {
                let _ = DestroyWindow(frame);
            }
            return;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let crop = RECT {
        left: rect.left - mon.left,
        top: rect.top - mon.top,
        right: rect.right - mon.left,
        bottom: rect.bottom - mon.top,
    };
    let worker = {
        let stop = stop.clone();
        let m = monitor.0 as isize;
        std::thread::spawn(move || {
            capture_loop(
                HMONITOR(m as _),
                crop,
                cursor,
                fps,
                thumb_size,
                &stop,
                started,
            )
            .map_err(|e| e.message())
        })
    };
    let outer = grow(rect, frame_width(s));
    let lay = layout_now(limit, s);
    let h = px(40.0, s);
    let at = place_bar(outer, rect, lay.w, h, px(8.0, s), work);
    let bar_rect = RECT {
        left: at.x,
        top: at.y,
        right: at.x + lay.w,
        bottom: at.y + h,
    };
    REC.with(|r| {
        *r.borrow_mut() = Some(Rec {
            rect,
            started,
            limit,
            stop,
            worker: Some(worker),
            frame,
            _frame_surface: frame_surface,
            bar,
            tip,
            s,
            work,
            bar_rect,
            hover: None,
            pressed: false,
        });
    });
    with_rec(|r| {
        r.bar.show_at(r.bar_rect);
        unsafe {
            SetTimer(Some(r.bar.hwnd), TIMER_TICK, 100, None);
        }
    });
    paint_bar();
    eprintln!(
        "record: start {}x{} at ({}, {}), fps {fps}, limit {}s, cursor {cursor}, bar {}x{} at ({}, {})",
        rect.right - rect.left,
        rect.bottom - rect.top,
        rect.left,
        rect.top,
        limit.as_secs(),
        lay.w,
        h,
        at.x,
        at.y
    );
}

fn with_rec<R>(f: impl FnOnce(&mut Rec) -> R) -> Option<R> {
    REC.with(|r| r.try_borrow_mut().ok()?.as_mut().map(f))
}

/// 録画枠の窓: 矩形の外 4 を囲む透明な窓。クリックは下へ通す (`WS_EX_LAYERED | WS_EX_TRANSPARENT`)。
fn frame_window(rect: RECT, s: f32) -> windows::core::Result<(HWND, Surface)> {
    let fw = frame_width(s);
    let outer = grow(rect, fw);
    let (w, h) = (outer.right - outer.left, outer.bottom - outer.top);
    unsafe {
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
            FRAME_CLASS,
            &HSTRING::from(crate::PRODUCT_NAME),
            WS_POPUP,
            outer.left,
            outer.top,
            w,
            h,
            None,
            None,
            GetModuleHandleW(None).ok().map(Into::into),
            None,
        )?;
        let _ = widgets::set_attr(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_DONOTROUND);
        ui::always_exclude_from_capture(hwnd);
        let surface = Surface::new(hwnd, w as u32, h as u32)?;
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            ex | (WS_EX_LAYERED | WS_EX_TRANSPARENT).0 as isize,
        );
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
        surface.draw(|ctx| {
            let mut d = 0;
            for (lw, color) in frame_lines(s) {
                let l = (fw - d - lw) as f32;
                let (r, b) = ((w - fw + d + lw) as f32, (h - fw + d + lw) as f32);
                let wf = lw as f32;
                // 矩形の外の、内から d の位置に太さ lw の輪。
                for band in [
                    rectf(l, l, r, l + wf),
                    rectf(l, b - wf, r, b),
                    rectf(l, l, l + wf, b),
                    rectf(r - wf, l, r, b),
                ] {
                    ui::fill_rect(ctx, band, color);
                }
                d += lw;
            }
        });
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            outer.left,
            outer.top,
            w,
            h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        Ok((hwnd, surface))
    }
}

fn paint_bar() {
    let pal = ui::palette();
    let t = crate::strings();
    with_rec(|r| {
        let s = r.s;
        let lay = layout_now(r.limit, s);
        let left = r.limit.saturating_sub(r.started.elapsed());
        let low = left <= Duration::from_secs(10);
        let hover = r.hover;
        let pressed = r.pressed;
        r.bar.paint(pal, |ctx| {
            let label = ui::text(t.remaining, Style::Caption, s);
            let time = ui::text(&clock(left), Style::NumStrong, s);
            let rowh = (lay.row.bottom - lay.row.top) as f32;
            let top = lay.row.top as f32;
            if low {
                let d = px(6.0, s) as f32;
                let y = top + ((rowh - d) / 2.0).round();
                fill_round(
                    ctx,
                    rectf(lay.dot_x, y, lay.dot_x + d, y + d),
                    d / 2.0,
                    pal.danger,
                );
            }
            draw_text(
                ctx,
                &label,
                lay.label_x,
                top + ((rowh - label.h) / 2.0).round(),
                pal.text_secondary,
            );
            draw_text(
                ctx,
                &time,
                lay.time.left as f32,
                top + ((rowh - time.h) / 2.0).round(),
                pal.text_primary,
            );
            let st = lay.stop;
            let fill = if hover == Some(Hot::Stop) || pressed {
                ui::REC_HOVER
            } else {
                ui::REC
            };
            let sr = rectf(
                st.left as f32,
                st.top as f32,
                st.right as f32,
                st.bottom as f32,
            );
            fill_round(ctx, sr, px(4.0, s) as f32, fill);
            let icon = px(16.0, s) as f32;
            let off = ((sr.right - sr.left - icon) / 2.0).round();
            widgets::draw_icon(ctx, "stop", sr.left + off, sr.top + off, icon, ui::ON_REC);
        });
    });
}

/// 経過のツールチップ: 残りの時間にホバーしている間、バーの下 8 に残りの段の中央で出す。
fn update_tip() {
    let pal = ui::palette();
    let t = crate::strings();
    with_rec(|r| {
        if r.hover != Some(Hot::Time) {
            r.tip.hide();
            return;
        }
        let s = r.s;
        let lay = layout_now(r.limit, s);
        let elapsed = Duration::from_secs(r.started.elapsed().as_secs());
        let tip = Tooltip::new(t.elapsed, &clock(elapsed), s);
        let target = RECT {
            left: r.bar_rect.left + lay.row.left,
            top: r.bar_rect.top,
            right: r.bar_rect.left + lay.row.right,
            bottom: r.bar_rect.bottom,
        };
        let at = place_tooltip(target, tip.w, tip.h, px(8.0, s), r.work);
        r.tip.show_at(RECT {
            left: at.x,
            top: at.y,
            right: at.x + tip.w,
            bottom: at.y + tip.h,
        });
        r.tip.paint(pal, |ctx| tip.draw(ctx, pal, s));
    });
}

/// 録画を止め、GIF ピンを出す。
pub fn stop() {
    let Some(mut rec) = REC.with(|r| r.try_borrow_mut().ok().and_then(|mut r| r.take())) else {
        return;
    };
    rec.stop.store(true, Ordering::Relaxed);
    let result = rec
        .worker
        .take()
        .map(|w| w.join().unwrap_or_else(|_| Err("worker panicked".into())));
    unsafe {
        let _ = KillTimer(Some(rec.bar.hwnd), TIMER_TICK);
        let _ = DestroyWindow(rec.tip.hwnd);
        let _ = DestroyWindow(rec.bar.hwnd);
        let _ = DestroyWindow(rec.frame);
    }
    match result {
        Some(Ok(done)) if done.store.len() > 0 => {
            eprintln!(
                "record: stop after {:.2} s: {} frames, {:.1} MB held",
                rec.started.elapsed().as_secs_f64(),
                done.store.len(),
                done.store.bytes() as f64 / 1_048_576.0
            );
            crate::gifpin::create(done, rec.rect);
        }
        Some(Ok(_)) => eprintln!("record: stop: no frames"),
        Some(Err(e)) => {
            eprintln!("record: failed: {e}");
            crate::notify(crate::tray_hwnd(), crate::strings().capture_failed);
        }
        None => {}
    }
}

/// 録画スレッド: 届いたフレームを fps で間引き、矩形を切り出して貯める。止めた時刻を `Recorded.end` に載せ、
/// 最後のフレームの表示時間が止めた時刻まで続くようにする (画面が動かないと WGC はフレームを送らない)。
fn capture_loop(
    monitor: HMONITOR,
    crop: RECT,
    cursor: bool,
    fps: u32,
    thumb_size: (u32, u32),
    stop: &AtomicBool,
    started: Instant,
) -> windows::core::Result<Recorded> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut d3d = None;
        let mut ctx = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_0]),
            D3D11_SDK_VERSION,
            Some(&mut d3d),
            None,
            Some(&mut ctx),
        )?;
        let d3d: ID3D11Device = d3d.unwrap();
        let ctx: ID3D11DeviceContext = ctx.unwrap();
        let device: IDirect3DDevice =
            CreateDirect3D11DeviceFromDXGIDevice(&d3d.cast::<IDXGIDevice>()?)?.cast()?;
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = interop.CreateForMonitor(monitor)?;
        let size = item.Size()?;
        // HDR のモニタは fp16 で撮り、コマごとに SDR へ変換する (`capture::freeze` と同じ。BGRA8 だと白飛びする)。
        let lut = crate::capture::sdr_white(monitor).map(crate::capture::sdr_lut);
        let (format, dxgi_format) = if lut.is_some() {
            (
                DirectXPixelFormat::R16G16B16A16Float,
                windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT,
            )
        } else {
            (
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            )
        };
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&device, format, 2, size)?;
        let session: GraphicsCaptureSession = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(cursor);
        if ApiInformation::IsPropertyPresent(
            h!("Windows.Graphics.Capture.GraphicsCaptureSession"),
            h!("IsBorderRequired"),
        )
        .unwrap_or(false)
        {
            let _ = session.SetIsBorderRequired(false);
        }
        // 切り出しはフレームの大きさに収める。
        let crop = RECT {
            left: crop.left.clamp(0, size.Width),
            top: crop.top.clamp(0, size.Height),
            right: crop.right.clamp(0, size.Width),
            bottom: crop.bottom.clamp(0, size.Height),
        };
        let (w, h) = (
            (crop.right - crop.left).max(1) as u32,
            (crop.bottom - crop.top).max(1) as u32,
        );
        let mut staging = None;
        d3d.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: dxgi_format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            },
            None,
            Some(&mut staging),
        )?;
        let staging = staging.unwrap();
        session.StartCapture()?;

        // キーフレームは 1 秒ごと (シークの復元は最大 1 秒ぶんの差分)。
        let mut store = FrameStore::new(w, h, fps as usize);
        let mut ts = Vec::new();
        let mut thumbs = Vec::new();
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let row = w as usize * 4;
        // コマは 1/fps 秒の刻みで採る (届いた時刻では採らない)。WGC は画面が変わったときだけ送るので、
        // 届いた順に並べるとコマの間隔がばらつく。刻みでは届いている中で一番新しいフレームを写し、
        // 無ければ前のコマを繰り返す。最初の刻みは最初のフレームが届いた時刻。
        let interval = Duration::from_secs(1) / fps;
        let mut first: Option<Duration> = None;
        let mut k = 0u32;
        let mut latest = None;
        // `buf` が前のコマから変わった (サムネイルを作り直す)。
        let mut fresh = false;
        // コマ落ちの切り分け (`record: capture` の 1 行と insight の `rec_arrive_gap` / `rec_late` / `rec_map`)。
        // arrived: WGC が届けた数。dropped: 読む前に drain で閉じた数 (このスレッドが遅れた証拠)。
        // repeated: 届いていなくて前のコマを繰り返した刻み (WGC が届けなかった証拠)。
        // gap: 到着の間隔の最大 (`SystemRelativeTime`)。late: 刻みに対する遅れの最大。
        let (mut arrived, mut dropped, mut repeated) = (0u32, 0u32, 0u32);
        let (mut max_gap, mut max_late) = (Duration::ZERO, Duration::ZERO);
        let mut last_arrival: Option<Duration> = None;
        while !stop.load(Ordering::Relaxed) {
            // 古いフレームは閉じて枠を空ける (枠は 2 つ: 持っていると次が届かない)。
            while let Ok(f) = pool.TryGetNextFrame() {
                arrived += 1;
                if let Ok(ts) = f.SystemRelativeTime() {
                    let at = Duration::from_nanos(ts.Duration.max(0) as u64 * 100);
                    if let Some(prev) = last_arrival {
                        let gap = at.saturating_sub(prev);
                        max_gap = max_gap.max(gap);
                        crate::insight_sample!("rec_arrive_gap", gap);
                    }
                    last_arrival = Some(at);
                }
                if let Some(old) = latest.replace(f) {
                    dropped += 1;
                    let _ = old.Close();
                }
            }
            let now = started.elapsed();
            if first.is_none() && latest.is_some() {
                first = Some(now);
            }
            let Some(t0) = first else {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            };
            let t = t0 + interval * k;
            if now < t {
                std::thread::sleep((t - now).min(Duration::from_millis(2)));
                continue;
            }
            k += 1;
            let late = now - t;
            max_late = max_late.max(late);
            crate::insight_sample!("rec_late", late);
            if latest.is_none() {
                repeated += 1;
            }
            if let Some(frame) = latest.take() {
                crate::insight_scope!("rec_frame");
                let texture: ID3D11Texture2D = frame
                    .Surface()?
                    .cast::<IDirect3DDxgiInterfaceAccess>()?
                    .GetInterface()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                // モニタの大きさが録画の途中で変わったら、その後は前のコマを繰り返す。
                if desc.Width as i32 == size.Width && desc.Height as i32 == size.Height {
                    ctx.CopySubresourceRegion(
                        &staging,
                        0,
                        0,
                        0,
                        0,
                        &texture,
                        0,
                        Some(&D3D11_BOX {
                            left: crop.left as u32,
                            top: crop.top as u32,
                            front: 0,
                            right: crop.left as u32 + w,
                            bottom: crop.top as u32 + h,
                            back: 1,
                        }),
                    );
                    let _ = frame.Close();
                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    {
                        // staging が 1 枚なので、ここで直前のコピーの GPU 完了を待つ。
                        crate::insight_scope!("rec_map");
                        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                    }
                    for y in 0..h as usize {
                        let at = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                        let dst = &mut buf[y * row..(y + 1) * row];
                        match &lut {
                            None => dst.copy_from_slice(std::slice::from_raw_parts(at, row)),
                            // fp16 の RGBA → BGRA8。
                            Some(lut) => {
                                let src = std::slice::from_raw_parts(at as *const u16, row);
                                for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                                    d[0] = lut[s[2] as usize];
                                    d[1] = lut[s[1] as usize];
                                    d[2] = lut[s[0] as usize];
                                }
                            }
                        }
                    }
                    ctx.Unmap(&staging, 0);
                    // デスクトップの α は不定のことがある。不透明にそろえる。
                    buf.chunks_exact_mut(4).for_each(|p| p[3] = 255);
                    fresh = true;
                } else {
                    let _ = frame.Close();
                }
            }
            {
                crate::insight_scope!("rec_store_push");
                store.push(&buf, t);
            }
            ts.push(t);
            let thumb = match thumbs.last() {
                Some(last) if !std::mem::take(&mut fresh) => Vec::clone(last),
                _ => thumbnail(&buf, w, h, thumb_size),
            };
            thumbs.push(thumb);
        }
        if let Some(f) = latest {
            let _ = f.Close();
        }
        // 刻み k 回に対して壁時計が何秒進んだか。遅れ続けると刻みの時刻 (`ts`) が壁時計より短くなる。
        let wall = first.map_or(Duration::ZERO, |t0| started.elapsed() - t0);
        eprintln!(
            "record: capture: {} ticks over {:.2} s wall ({:.2} s nominal), {arrived} arrived, {dropped} dropped in drain, {repeated} repeated, max gap {:.1} ms, max late {:.1} ms",
            ts.len(),
            wall.as_secs_f64(),
            (interval * k).as_secs_f64(),
            max_gap.as_secs_f64() * 1000.0,
            max_late.as_secs_f64() * 1000.0,
        );
        // 最後のコマも 1 刻みぶん出す (どのコマも同じ長さ)。
        let end = ts.last().map_or(Duration::ZERO, |&last| last + interval);
        let _ = session.Close();
        let _ = pool.Close();
        debug_assert!(
            store.len() == ts.len()
                && thumbs.len() == ts.len()
                && ts.windows(2).all(|p| p[0] < p[1]),
            "record: frames, times and thumbs must pair up"
        );
        Ok(Recorded {
            store,
            ts,
            end,
            thumbs,
            thumb_size,
        })
    }
}

/// サムネイル: 枠いっぱいに覆う大きさに縮め、中央を切り出す。
/// 1 画素ごとに元の範囲から最大 4 × 4 点を拾って平均する (ニアレストだと細部がつぶれる)。
/// ponytail: 4 × 4 で頭打ち。1/16 より縮むと間引きが残る。録画の毎コマで呼ぶので全画素の平均はしない。
pub(crate) fn thumbnail(bgra: &[u8], w: u32, h: u32, (tw, th): (u32, u32)) -> Vec<u8> {
    let k = (tw as f32 / w as f32).max(th as f32 / h as f32);
    let (ox, oy) = (
        (w as f32 * k - tw as f32) / 2.0,
        (h as f32 * k - th as f32) / 2.0,
    );
    let n = ((1.0 / k).ceil() as u32).clamp(1, 4);
    let mut out = Vec::with_capacity((tw * th * 4) as usize);
    for y in 0..th {
        for x in 0..tw {
            let mut acc = [0u32; 4];
            for j in 0..n {
                let sy = (((y as f32 + (j as f32 + 0.5) / n as f32 + oy) / k) as u32).min(h - 1);
                for i in 0..n {
                    let sx =
                        (((x as f32 + (i as f32 + 0.5) / n as f32 + ox) / k) as u32).min(w - 1);
                    let at = ((sy * w + sx) * 4) as usize;
                    for c in 0..4 {
                        acc[c] += u32::from(bgra[at + c]);
                    }
                }
            }
            out.extend(acc.map(|v| (v / (n * n)) as u8));
        }
    }
    out
}

fn hit(p: POINT) -> Option<Hot> {
    with_rec(|r| {
        let lay = layout_now(r.limit, r.s);
        let inside = |q: RECT| p.x >= q.left && p.x < q.right && p.y >= q.top && p.y < q.bottom;
        if inside(lay.stop) {
            Some(Hot::Stop)
        } else if inside(lay.time) {
            Some(Hot::Time)
        } else {
            None
        }
    })
    .flatten()
}

unsafe extern "system" fn frame_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_PAINT => {
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe extern "system" fn bar_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let is_bar = with_rec(|r| r.bar.hwnd == hwnd).unwrap_or(false);
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_SETCURSOR => {
            unsafe {
                SetCursor(LoadCursorW(None, IDC_ARROW).ok());
            }
            LRESULT(1)
        }
        WM_TIMER if is_bar && wparam.0 == TIMER_TICK => {
            let over = with_rec(|r| r.started.elapsed() >= r.limit).unwrap_or(false);
            if over {
                eprintln!("record: limit reached");
                stop();
            } else {
                paint_bar();
                update_tip();
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE if is_bar => {
            let h = hit(lparam_point(lparam));
            let changed = with_rec(|r| std::mem::replace(&mut r.hover, h) != h).unwrap_or(false);
            track_leave(hwnd);
            if changed {
                paint_bar();
                update_tip();
            }
            LRESULT(0)
        }
        0x02A3 /* WM_MOUSELEAVE */ if is_bar => {
            with_rec(|r| r.hover = None);
            paint_bar();
            update_tip();
            LRESULT(0)
        }
        WM_LBUTTONDOWN if is_bar => {
            if hit(lparam_point(lparam)) == Some(Hot::Stop) {
                with_rec(|r| r.pressed = true);
                unsafe {
                    SetCapture(hwnd);
                }
                paint_bar();
            }
            LRESULT(0)
        }
        WM_LBUTTONUP if is_bar => {
            let pressed = with_rec(|r| std::mem::take(&mut r.pressed)).unwrap_or(false);
            unsafe {
                let _ = ReleaseCapture();
            }
            if pressed && hit(lparam_point(lparam)) == Some(Hot::Stop) {
                eprintln!("record: stop button");
                stop();
            } else {
                paint_bar();
            }
            LRESULT(0)
        }
        WM_PAINT => {
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_dimensions() {
        // 見本は約 120: 4 + (8 + 6 + 6 + 「残り」24 + 6 + 「0:48」26 + 8) + 2 + 32 + 4。
        let lay = bar_layout(24.0, 26.0, 1.0);
        assert_eq!(lay.w, 126);
        assert_eq!((lay.stop.left, lay.stop.right), (90, 122));
        assert_eq!(lay.stop.bottom - lay.stop.top, 32);
        // 置き方: 下 8 → 上 8 → 矩形の内側の右下 8。
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let rect = RECT {
            left: 100,
            top: 100,
            right: 500,
            bottom: 400,
        };
        let outer = grow(rect, 4);
        assert_eq!(
            place_bar(outer, rect, 126, 40, 8, work),
            POINT { x: 378, y: 412 }
        );
        let tall = RECT {
            left: 100,
            top: 100,
            right: 500,
            bottom: 1020,
        };
        assert_eq!(
            place_bar(grow(tall, 4), tall, 126, 40, 8, work).y,
            100 - 4 - 8 - 40
        );
        let full = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        assert_eq!(
            place_bar(grow(full, 4), full, 126, 40, 8, work),
            POINT {
                x: 1920 - 8 - 126,
                y: 1040 - 8 - 40
            }
        );
    }
}
