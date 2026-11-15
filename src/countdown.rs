//! 録画の前のカウントダウン (秒数は設定) (DS `RecordCountdown`)。選んだ範囲の中央に出す acrylic の窓。
//! 出している間だけ LL フックで Esc (中止) と Enter (今すぐ開始) を受ける (クリックで前面が移っても効く)。
//! クリックは下の窓へ通す。録画には写らない。

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Instant;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_RETURN, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::ui::anim;
use crate::ui::widgets::{self, Acrylic, Capture};
use crate::ui::{self, Style, Text, draw_text, fill_round, px, rectf, stroke};

const CLASS: PCWSTR = w!("OdileCountdown");
const TIMER_TICK: usize = 1;
/// 円弧を滑らかに減らす再描画 (アニメーション効果がオンのときだけ)。
const TIMER_ANIM: usize = 2;

type Done = Box<dyn FnOnce(bool)>;

struct Cd {
    win: Acrylic,
    s: f32,
    /// 数え始めの秒数。
    secs: u32,
    left: u32,
    /// 始めた時刻。円弧はここからの経過で減らす。
    t0: Instant,
    /// Windows のアニメーション効果。オフなら円弧も 1 秒ごとに減らす。
    motion: bool,
    done: Option<Done>,
}

thread_local! {
    static CD: RefCell<Option<Cd>> = const { RefCell::new(None) };
    static KEY_HOOK: Cell<Option<HHOOK>> = const { Cell::new(None) };
}

/// フックが Esc と Enter を送る窓。0 ならカウントダウンしていない。
static KEY_HWND: AtomicIsize = AtomicIsize::new(0);

pub fn active() -> bool {
    CD.with(|c| c.try_borrow().map(|c| c.is_some()).unwrap_or(true))
}

/// 窓の左上: 中央を範囲の中央に合わせ、作業領域に収める。範囲より大きくても縮めない。
fn place(rect: RECT, w: i32, h: i32, work: RECT) -> POINT {
    let cx = (rect.left + rect.right) / 2;
    let cy = (rect.top + rect.bottom) / 2;
    POINT {
        x: (cx - w / 2).min(work.right - w).max(work.left),
        y: (cy - h / 2).min(work.bottom - h).max(work.top),
    }
}

/// 案内の行「[Esc] 中止 [Enter] 今すぐ開始」の部品と幅。
struct Guide {
    esc: Text,
    cancel: Text,
    enter: Text,
    start: Text,
    w: f32,
}

fn keycap_w(t: &Text, s: f32) -> f32 {
    t.w + 2.0 * px(8.0, s) as f32
}

fn guide(s: f32) -> Guide {
    let t = crate::strings();
    let esc = ui::text("Esc", Style::CaptionStrong, s);
    let cancel = ui::text(t.countdown_cancel, Style::Caption, s);
    let enter = ui::text("Enter", Style::CaptionStrong, s);
    let start = ui::text(t.countdown_start_now, Style::Caption, s);
    let (g6, g16) = (px(6.0, s) as f32, px(16.0, s) as f32);
    let w = keycap_w(&esc, s) + g6 + cancel.w + g16 + keycap_w(&enter, s) + g6 + start.w;
    Guide {
        esc,
        cancel,
        enter,
        start,
        w,
    }
}

/// 窓の外形: 幅 = 案内の行 + 16 × 2、高さ 16 + 円 120 + 12 + 案内 22 + 16。
fn size(s: f32) -> (i32, i32) {
    let w = guide(s).w.ceil() as i32 + 2 * px(16.0, s);
    let h = 2 * px(16.0, s) + px(120.0, s) + px(12.0, s) + px(22.0, s);
    (w, h)
}

/// `rect` (録画する矩形) の中央にカウントダウンを出す。0 か Enter で `done(true)`、Esc か `cancel` で `done(false)`。
/// `work` は範囲のモニタの作業領域、`s` はその倍率。
pub fn start(secs: u32, rect: RECT, work: RECT, s: f32, done: Done) {
    if active() {
        done(false);
        return;
    }
    widgets::register_class(CLASS, proc);
    let mut win = match Acrylic::new(CLASS, false, Capture::Never) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("countdown: window failed: {e}");
            done(true);
            return;
        }
    };
    let hwnd = win.hwnd;
    unsafe {
        // 前面を取って Esc と Enter を受ける (`Acrylic::new` は NOACTIVATE を付ける)。クリックは下へ通す。
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let ex =
            (ex & !(WS_EX_NOACTIVATE.0 as isize)) | (WS_EX_LAYERED | WS_EX_TRANSPARENT).0 as isize;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
    }
    let (w, h) = size(s);
    let at = place(rect, w, h, work);
    win.show_at(ui::rect_at(at, w, h));
    CD.with(|c| {
        *c.borrow_mut() = Some(Cd {
            win,
            s,
            secs,
            left: secs,
            t0: Instant::now(),
            motion: anim::motion(),
            done: Some(done),
        })
    });
    paint();
    key_hook(Some(hwnd));
    // フックが入らなかったときの経路: 前面を取り、窓の WM_KEYDOWN で受ける。
    crate::force_foreground(hwnd);
    unsafe {
        SetTimer(Some(hwnd), TIMER_TICK, 1000, None);
        if CD.with(|c| c.borrow().as_ref().is_some_and(|cd| cd.motion)) {
            SetTimer(Some(hwnd), TIMER_ANIM, 16, None);
        }
    }
    let fg = unsafe { GetForegroundWindow() } == hwnd;
    eprintln!(
        "countdown: start {w}x{h} at ({}, {}), foreground {fg}",
        at.x, at.y
    );
}

/// 録画を始めずに終える (動画のホットキー)。
pub fn cancel() {
    finish(End::Cancel);
}

#[derive(Clone, Copy, Debug)]
enum End {
    /// 0 になった。
    Start,
    /// Enter。
    Skip,
    /// Esc・動画のホットキー。
    Cancel,
}

/// 窓を閉じてから `done` を呼ぶ (録画の最初のフレームに窓を残さない)。
fn finish(end: End) {
    let Some(cd) = CD.with(|c| c.try_borrow_mut().ok().and_then(|mut c| c.take())) else {
        return;
    };
    key_hook(None);
    unsafe {
        let _ = KillTimer(Some(cd.win.hwnd), TIMER_TICK);
        let _ = KillTimer(Some(cd.win.hwnd), TIMER_ANIM);
        let _ = DestroyWindow(cd.win.hwnd);
    }
    let tag = match end {
        End::Start => "start",
        End::Skip => "skip",
        End::Cancel => "cancel",
    };
    eprintln!("countdown: done ({tag})");
    if let Some(done) = cd.done {
        done(!matches!(end, End::Cancel));
    }
}

/// カウントダウンの間だけ UI スレッドに LL フックを入れる (`setwin::rec_hook` と同じ形)。
/// `None` で外す。
fn key_hook(target: Option<HWND>) {
    KEY_HOOK.with(|h| {
        if let Some(old) = h.take() {
            let _ = unsafe { UnhookWindowsHookEx(old) };
            eprintln!("countdown: key hook off");
        }
        KEY_HWND.store(target.map_or(0, |w| w.0 as isize), Ordering::Relaxed);
        if target.is_some() {
            match unsafe {
                SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(key_hook_proc),
                    GetModuleHandleW(None).ok().map(Into::into),
                    0,
                )
            } {
                Ok(hook) => {
                    h.set(Some(hook));
                    eprintln!("countdown: key hook on");
                }
                Err(e) => eprintln!("countdown: key hook failed: {e}"),
            }
        }
    });
}

/// 修飾なしの Esc と Enter の KEYDOWN だけを飲み、窓へ `WM_KEYDOWN` で送る。
/// Alt 付きは `WM_SYSKEYDOWN` なので通る。Ctrl+Esc なども飲まない。他のキーは全部流す。
unsafe extern "system" fn key_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let target = KEY_HWND.load(Ordering::Relaxed);
    if code == HC_ACTION as i32 && target != 0 && wparam.0 as u32 == WM_KEYDOWN {
        let vk = unsafe { (*(lparam.0 as *const KBDLLHOOKSTRUCT)).vkCode };
        let held = |vk: u16| unsafe { GetAsyncKeyState(vk as i32) } < 0;
        if (vk == u32::from(VK_ESCAPE.0) || vk == u32::from(VK_RETURN.0))
            && ![VK_CONTROL, VK_SHIFT, VK_LWIN, VK_RWIN]
                .iter()
                .any(|k| held(k.0))
        {
            let _ = unsafe {
                PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_KEYDOWN,
                    WPARAM(vk as usize),
                    LPARAM(0),
                )
            };
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 数字と円弧を描く。数字は 1 秒ごと、円弧は経過時間で滑らかに減る (アニメーション効果がオフなら 1 秒ごと)。
fn paint() {
    let pal = ui::palette();
    CD.with(|c| {
        let c = c.borrow();
        let Some(cd) = c.as_ref() else {
            return;
        };
        let s = cd.s;
        let g = guide(s);
        let w = cd.win.surface.w as f32;
        let pad = px(16.0, s) as f32;
        let d = px(120.0, s) as f32;
        let sw = stroke(4.0, s) as f32;
        let left_x = ((w - d) / 2.0).round();
        let (cx, cy) = (left_x + d / 2.0, pad + d / 2.0);
        let r = (d - sw) / 2.0;
        let frac = if cd.motion {
            (1.0 - cd.t0.elapsed().as_secs_f32() / cd.secs as f32).clamp(0.0, 1.0)
        } else {
            cd.left as f32 / cd.secs as f32
        };
        let num = ui::badge_text(&cd.left.to_string(), 64.0 * s);
        cd.win.paint(pal, |ctx| unsafe {
            let v2 = |x: f32, y: f32| windows_numerics::Vector2 { X: x, Y: y };
            let ring = D2D1_ELLIPSE {
                point: v2(cx, cy),
                radiusX: r,
                radiusY: r,
            };
            if let Ok(b) = ctx.CreateSolidColorBrush(&pal.track, None) {
                ctx.DrawEllipse(&ring, &b, sw, None);
            }
            if let Ok(b) = ctx.CreateSolidColorBrush(&pal.on_fill, None) {
                if frac >= 1.0 {
                    // 始点へ戻る円弧は描かれないので、全周は円で描く。
                    ctx.DrawEllipse(&ring, &b, sw, None);
                } else if frac > 0.0 {
                    // 12 時から時計回りに残りの割合。
                    let a = std::f32::consts::TAU * frac;
                    let (x, y) = (cx + r * a.sin(), cy - r * a.cos());
                    let large = u8::from(frac > 0.5);
                    let path = format!("M {cx} {} A {r} {r} 0 {large} 1 {x} {y}", cy - r);
                    if let Ok(geo) = widgets::geometry(&path) {
                        ctx.DrawGeometry(&geo, &b, sw, &widgets::round_stroke());
                    }
                }
            }
            // 字面の中央を円の中央に: 行の箱を円の中央に置いて 4 上げる (DS)。
            draw_text(
                ctx,
                &num,
                ((w - num.w) / 2.0).round(),
                (cy - num.h / 2.0).round() - px(4.0, s) as f32,
                pal.text_primary,
            );
            // 案内の行: 円の下 12、左右の中央。
            let kh = px(22.0, s) as f32;
            let top = pad + d + px(12.0, s) as f32;
            let mut x = ((w - g.w) / 2.0).round();
            for (key, label, gap) in [
                (&g.esc, &g.cancel, px(16.0, s) as f32),
                (&g.enter, &g.start, 0.0),
            ] {
                let kw = keycap_w(key, s);
                fill_round(
                    ctx,
                    rectf(x, top, x + kw, top + kh),
                    px(4.0, s) as f32,
                    pal.keycap,
                );
                draw_text(
                    ctx,
                    key,
                    x + px(8.0, s) as f32,
                    top + ((kh - key.h) / 2.0).round(),
                    pal.text_primary,
                );
                x += kw + px(6.0, s) as f32;
                draw_text(
                    ctx,
                    label,
                    x,
                    top + ((kh - label.h) / 2.0).round(),
                    pal.text_secondary,
                );
                x += label.w + gap;
            }
        });
    });
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wparam.0 == TIMER_TICK => {
            let left = CD.with(|c| {
                c.borrow_mut().as_mut().map(|cd| {
                    cd.left = cd.left.saturating_sub(1);
                    cd.left
                })
            });
            match left {
                Some(0) => finish(End::Start),
                Some(_) => paint(),
                None => {}
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_ANIM => {
            paint();
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == usize::from(VK_ESCAPE.0) => {
            finish(End::Cancel);
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == usize::from(VK_RETURN.0) => {
            finish(End::Skip);
            LRESULT(0)
        }
        // Alt+F4: Esc と同じく取り消す。窓だけ壊れるとフックと録画枠が残る。
        WM_CLOSE => {
            finish(End::Cancel);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn place_centers_clamps_and_never_shrinks() {
        let work = r(0, 0, 1920, 1032);
        let (w, h) = (217, 186);
        // (範囲, 左上): モニタの中央・左上の端・右下の端・窓より小さい範囲・作業領域の外へ出る範囲。
        for (rect, at) in [
            (r(640, 360, 1280, 720), (852, 447)),
            (r(0, 0, 100, 60), (0, 0)),
            (r(1800, 980, 1920, 1032), (1703, 846)),
            (r(900, 500, 1000, 560), (842, 437)),
            (r(1700, 900, 1920, 1080), (1702, 846)),
        ] {
            assert_eq!(
                place(rect, w, h, work),
                POINT { x: at.0, y: at.1 },
                "{rect:?}"
            );
        }
        // 第 2 モニタ (x 1920..3840) の作業領域にも収める。
        let work2 = r(1920, 0, 3840, 1032);
        assert_eq!(
            place(r(1900, 0, 2000, 100), w, h, work2),
            POINT { x: 1920, y: 0 }
        );
    }
}
