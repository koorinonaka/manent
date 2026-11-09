//! 範囲選択のオーバーレイ。モニタごとに 1 枚の窓 (DPI の混在に合わせ、ラベルと線をそのモニタの s で描く)。
//! 静止画は凍結画像を背景にし、動画 (a6ed) は凍結無し (`shots` が `None`) で同じ選択を使う。
//! ホバーで窓 → UI 要素 (UIA、専用スレッド) をハイライトし、ホイールで親子を辿る。
//! ドラッグで自由矩形を作り、辺を窓と UI 要素の辺に吸着させる。

use std::cell::RefCell;
use std::sync::mpsc::Sender;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, w};

use crate::capture::Shot;
use crate::layout::{Edge, Side, front_edges, place_label, rect_of, snap};
use crate::ui::anim::{self, Anim};
use crate::ui::widgets::{Label, Part};
use crate::ui::{
    self, Palette, Style, Surface, contains, cursor, fill_rect, frame_bounds, px, rectf, rf,
    stroke, stroke_rect,
};
use crate::uia;

/// 窓の矩形 (影を除く) と題名。Z 順 (手前から)。
#[derive(Clone)]
pub struct WinInfo {
    pub hwnd: isize,
    pub rect: RECT,
    pub title: String,
}

/// 見えているトップレベル窓を Z 順で。不可視・最小化・クローク・自アプリの窓は除く。
pub fn top_windows() -> Vec<WinInfo> {
    unsafe extern "system" fn each(hwnd: HWND, data: LPARAM) -> windows::core::BOOL {
        let list = unsafe { &mut *(data.0 as *mut Vec<WinInfo>) };
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                return true.into();
            }
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut cloaked).cast(), 4);
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if cloaked != 0 || pid == GetCurrentProcessId() {
                return true.into();
            }
            let rect = frame_bounds(hwnd);
            if rect.right - rect.left < 2 || rect.bottom - rect.top < 2 {
                return true.into();
            }
            let mut buf = [0u16; 256];
            let n = GetWindowTextW(hwnd, &mut buf);
            list.push(WinInfo {
                hwnd: hwnd.0 as isize,
                rect,
                title: String::from_utf16_lossy(&buf[..n.max(0) as usize]),
            });
        }
        true.into()
    }
    let mut list = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&raw mut list as isize));
    }
    list
}

fn inflate(r: RECT, d: i32) -> RECT {
    RECT {
        left: r.left - d,
        top: r.top - d,
        right: r.right + d,
        bottom: r.bottom + d,
    }
}

fn intersect(a: RECT, b: RECT) -> RECT {
    RECT {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    }
}

struct Mon {
    hwnd: HWND,
    rect: RECT,
    s: f32,
    surface: Surface,
    bitmap: Option<ID2D1Bitmap1>,
}

#[derive(Clone, Copy)]
struct Snapped {
    /// 縦の辺 (x に吸着)。
    vertical: bool,
    edge: Edge,
}

struct Drag {
    anchor: POINT,
    cur: POINT,
    /// 始点と終点がそれぞれ吸着した辺。
    anchor_snaps: Vec<Snapped>,
    snaps: Vec<Snapped>,
}

/// 選んだ結果。`shots` は凍結画像 (動画のときは空)。
pub struct Selection {
    pub rect: RECT,
    pub shots: Vec<Shot>,
}

type Done = Box<dyn FnOnce(Option<Selection>)>;

struct Overlay {
    mons: Vec<Mon>,
    shots: Vec<Shot>,
    windows: Vec<WinInfo>,
    hover: Option<usize>,
    /// UIA の答え (深い順) と、それを求めた窓の添字。
    chain: Vec<uia::Element>,
    chain_window: Option<usize>,
    /// `chain` の何段目を選んでいるか (0 が最深)。
    level: usize,
    press: Option<POINT>,
    drag: Option<Drag>,
    /// 吸着した辺 (縦か, 位置) の強調のフェード。
    snap_fade: RefCell<Anim<(bool, i32)>>,
    done: Option<Done>,
}

/// 吸着の強調のフェードのコマ。
const TIMER_SNAP: usize = 1;

thread_local! {
    static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) };
    static UIA: RefCell<Option<Sender<uia::Request>>> = const { RefCell::new(None) };
}

pub fn active() -> bool {
    OVERLAY.with(|o| o.try_borrow().map(|o| o.is_some()).unwrap_or(true))
}

const CLASS: windows::core::PCWSTR = w!("OdileOverlay");

/// オーバーレイを出す。`shots` が凍結画像 (静止画)、`None` なら生の画面の上で選ぶ (動画)。
/// 選び終わる (Esc・右クリックは `None`) と `done` を呼ぶ。
pub fn start(shots: Option<Vec<Shot>>, done: Done) {
    crate::insight_scope!("overlay_start");
    if active() {
        return;
    }
    ui::widgets::register_class(CLASS, wndproc);
    UIA.with(|u| {
        u.borrow_mut()
            .get_or_insert_with(|| uia::start(crate::tray_hwnd(), crate::WM_UIA));
    });
    let windows = top_windows();
    let frozen = shots.is_some();
    let shots = shots.unwrap_or_default();
    let mut mons = Vec::new();
    for (monitor, rect, _) in crate::capture::monitors() {
        let hwnd = match unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP,
                CLASS,
                &HSTRING::from(crate::PRODUCT_NAME),
                WS_POPUP,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                None,
                None,
                GetModuleHandleW(None).ok().map(Into::into),
                None,
            )
        } {
            Ok(h) => h,
            Err(e) => {
                eprintln!("overlay: window failed: {e}");
                continue;
            }
        };
        let _ = ui::widgets::set_attr(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_DONOTROUND);
        ui::always_exclude_from_capture(hwnd);
        // 生成時のモニタと違う DPI のモニタへ置くと、Windows が大きさを変えることがある。置き直す。
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        let surface = match Surface::new(
            hwnd,
            (rect.right - rect.left) as u32,
            (rect.bottom - rect.top) as u32,
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("overlay: surface failed: {e}");
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                continue;
            }
        };
        let bitmap = if frozen {
            shots
                .iter()
                .find(|s| s.monitor == monitor)
                .and_then(|shot| ui::bitmap_from_texture(&surface.ctx, &shot.texture).ok())
        } else {
            None
        };
        mons.push(Mon {
            hwnd,
            rect,
            s: ui::scale_of(hwnd),
            surface,
            bitmap,
        });
    }
    if mons.is_empty() {
        // 窓が 1 枚も無いと finish を呼ぶ経路が無く、active() が真のまま残る。
        eprintln!("overlay: no monitor window");
        done(None);
        return;
    }
    let overlay = Overlay {
        mons,
        shots,
        windows,
        hover: None,
        chain: Vec::new(),
        chain_window: None,
        level: 0,
        press: None,
        drag: None,
        snap_fade: RefCell::new(Anim::default()),
        done: Some(done),
    };
    let hwnds: Vec<HWND> = overlay.mons.iter().map(|m| m.hwnd).collect();
    OVERLAY.with(|o| *o.borrow_mut() = Some(overlay));
    update_hover();
    redraw_all();
    for &h in &hwnds {
        unsafe {
            let _ = ShowWindow(h, SW_SHOW);
        }
    }
    // Esc を受けるため、カーソルのあるモニタの窓を前面にする。
    let pt = cursor();
    let target = OVERLAY.with(|o| {
        o.borrow().as_ref().and_then(|o| {
            o.mons
                .iter()
                .find(|m| contains(&m.rect, pt))
                .map(|m| m.hwnd)
        })
    });
    if let Some(h) = target.or(hwnds.first().copied()) {
        crate::force_foreground(h);
    }
}

fn finish(rect: Option<RECT>) {
    let Some(mut o) = OVERLAY.with(|o| o.borrow_mut().take()) else {
        return;
    };
    let done = o.done.take();
    for m in &o.mons {
        unsafe {
            let _ = DestroyWindow(m.hwnd);
        }
    }
    let virt = o.virt();
    let selection = rect
        .zip(virt)
        .map(|(r, v)| intersect(r, v))
        .filter(|r| r.right - r.left > 0 && r.bottom - r.top > 0)
        .map(|rect| Selection {
            rect,
            shots: std::mem::take(&mut o.shots),
        });
    drop(o);
    if let Some(done) = done {
        done(selection);
    }
}

impl Overlay {
    /// 全モニタを囲む矩形 (仮想画面)。
    fn virt(&self) -> Option<RECT> {
        self.mons.iter().fold(None::<RECT>, |acc, m| {
            Some(match acc {
                None => m.rect,
                Some(a) => RECT {
                    left: a.left.min(m.rect.left),
                    top: a.top.min(m.rect.top),
                    right: a.right.max(m.rect.right),
                    bottom: a.bottom.max(m.rect.bottom),
                },
            })
        })
    }

    fn s_at(&self, p: POINT) -> f32 {
        self.mons
            .iter()
            .find(|m| contains(&m.rect, p))
            .map_or(1.0, |m| m.s)
    }

    /// 点のあるモニタの矩形 (ラベルが入るかの判定に使う)。
    fn monitor_at(&self, p: POINT) -> RECT {
        self.mons
            .iter()
            .find(|m| contains(&m.rect, p))
            .or(self.mons.first())
            .map_or(RECT::default(), |m| m.rect)
    }

    /// いま選ばれる矩形: ドラッグ中はその矩形、ホバー中は UI 要素か窓。
    fn target(&self) -> Option<RECT> {
        if let Some(d) = &self.drag {
            return Some(rect_of(d.anchor, d.cur));
        }
        let win = self.hover?;
        if self.chain_window == Some(win) && self.level < self.chain.len().saturating_sub(1) {
            return Some(self.chain[self.level].rect);
        }
        Some(self.windows[win].rect)
    }

    /// 吸着先: 点 `p` の近くで最前面に見えている窓・UI 要素の辺と、モニタの辺。(縦の辺, 横の辺)
    fn edges(&self, p: POINT) -> (Vec<Edge>, Vec<Edge>) {
        // UI 要素はその窓と同じ Z の位置に置く (窓より手前の窓に隠れる)。
        let at = self.chain_window.unwrap_or(self.windows.len());
        let rects: Vec<RECT> = self.windows[..at]
            .iter()
            .map(|w| w.rect)
            .chain(self.chain.iter().map(|e| e.rect))
            .chain(self.windows[at..].iter().map(|w| w.rect))
            .collect();
        let (mut v, mut h) = front_edges(&rects, p);
        for r in self.mons.iter().map(|m| m.rect) {
            v.push(Edge {
                pos: r.left,
                from: r.top,
                to: r.bottom,
            });
            v.push(Edge {
                pos: r.right,
                from: r.top,
                to: r.bottom,
            });
            h.push(Edge {
                pos: r.top,
                from: r.left,
                to: r.right,
            });
            h.push(Edge {
                pos: r.bottom,
                from: r.left,
                to: r.right,
            });
        }
        (v, h)
    }

    /// 点を辺に吸着させる。吸着した辺も返す。
    fn snap_point(&self, p: POINT) -> (POINT, Vec<Snapped>) {
        let th = px(8.0, self.s_at(p));
        let (v, h) = self.edges(p);
        let mut out = p;
        let mut snaps = Vec::new();
        if let Some(e) = snap(p.x, p.y, &v, th) {
            out.x = e.pos;
            snaps.push(Snapped {
                vertical: true,
                edge: e,
            });
        }
        if let Some(e) = snap(p.y, p.x, &h, th) {
            out.y = e.pos;
            snaps.push(Snapped {
                vertical: false,
                edge: e,
            });
        }
        (out, snaps)
    }
}

fn ctrl() -> bool {
    unsafe { GetKeyState(VK_CONTROL.0 as i32) < 0 }
}

fn update_hover() {
    crate::insight_scope!("overlay_hover");
    let pt = cursor();
    let request = OVERLAY.with(|o| {
        let mut o = o.borrow_mut();
        let o = o.as_mut()?;
        if let Some(press) = o.press {
            let s = o.s_at(pt);
            let moved = (pt.x - press.x).abs().max((pt.y - press.y).abs());
            if o.drag.is_some() || moved >= px(4.0, s) {
                // 吸着は Ctrl を押している間だけ。押す・離すで始点も終点も付け直す。
                let ((anchor, anchor_snaps), (cur, snaps)) = if ctrl() {
                    (o.snap_point(press), o.snap_point(pt))
                } else {
                    ((press, Vec::new()), (pt, Vec::new()))
                };
                o.drag = Some(Drag {
                    anchor,
                    cur,
                    anchor_snaps,
                    snaps,
                });
                return None;
            }
        }
        // 窓・UI 要素のハイライト (クリックで確定) も Ctrl を押している間だけ。
        let hover = if ctrl() {
            o.windows.iter().position(|w| contains(&w.rect, pt))
        } else {
            None
        };
        if hover != o.hover {
            o.hover = hover;
            o.level = 0;
        }
        let win = hover?;
        // UIA の答えを待たずに、点から外れた深い要素を捨てる (浅い段へすぐ戻る)。
        if o.chain_window == Some(win) {
            o.level = o.level.saturating_sub(trim_chain(&mut o.chain, pt));
        }
        Some(uia::Request {
            hwnd: o.windows[win].hwnd,
            pt,
        })
    });
    if let Some(req) = request {
        UIA.with(|u| {
            if let Some(tx) = u.borrow().as_ref() {
                let _ = tx.send(req);
            }
        });
    }
}

/// 深い順の `chain` から、点を含まない深い要素を落とす。最後 (窓) は残す。落とした数を返す。
fn trim_chain(chain: &mut Vec<uia::Element>, pt: POINT) -> usize {
    let keep_from = chain
        .iter()
        .position(|e| contains(&e.rect, pt))
        .unwrap_or(chain.len().saturating_sub(1));
    chain.drain(..keep_from);
    keep_from
}

/// UIA の答え (トレイの窓が `WM_UIA` で受けて渡す)。
pub fn on_uia(answer: Box<uia::Answer>) {
    eprintln!(
        "uia: {} element(s) in {:.1} ms",
        answer.chain.len(),
        answer.ms
    );
    let changed = OVERLAY.with(|o| {
        let mut o = o.borrow_mut();
        let Some(o) = o.as_mut() else { return false };
        // 古い点の答えでも、同じ窓なら使う (動き続けている間も判定を進める)。今の点から外れた所は捨てる。
        let Some(win) = o.hover else { return false };
        if o.windows[win].hwnd != answer.hwnd {
            return false;
        }
        let mut chain = answer.chain;
        trim_chain(&mut chain, cursor());
        // 最後 (窓そのもの) は UIA の矩形でなく影を除いた矩形にそろえる。
        if let (Some(last), Some(win)) = (chain.last_mut(), o.hover) {
            last.rect = o.windows[win].rect;
        }
        let same = o.chain.first().map(|e| e.rect) == chain.first().map(|e| e.rect)
            && o.chain_window == o.hover;
        if !same {
            o.level = 0;
        }
        o.chain = chain;
        o.chain_window = o.hover;
        true
    });
    if changed {
        redraw_all();
    }
}

fn redraw_all() {
    let pal = ui::palette();
    let strings = crate::strings();
    OVERLAY.with(|o| {
        let o = o.borrow();
        let Some(o) = o.as_ref() else { return };
        for m in &o.mons {
            draw_mon(o, m, pal, strings);
        }
    });
}

fn draw_mon(o: &Overlay, m: &Mon, pal: &Palette, strings: &crate::i18n::Strings) {
    crate::insight_scope!("overlay_draw_mon");
    let s = m.s;
    m.surface.draw(|ctx| unsafe {
        let origin =
            windows_numerics::Matrix3x2::translation(-m.rect.left as f32, -m.rect.top as f32);
        if let Some(b) = &m.bitmap {
            ctx.DrawBitmap(
                b,
                None,
                1.0,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                None,
                None,
            );
        }
        ctx.SetTransform(&origin);
        let big = m.rect;
        let target = o.target();
        // 暗幕: 対象の外だけ。
        match target {
            Some(t) => {
                for r in [
                    RECT {
                        left: big.left,
                        top: big.top,
                        right: big.right,
                        bottom: t.top,
                    },
                    RECT {
                        left: big.left,
                        top: t.bottom,
                        right: big.right,
                        bottom: big.bottom,
                    },
                    RECT {
                        left: big.left,
                        top: t.top,
                        right: t.left,
                        bottom: t.bottom,
                    },
                    RECT {
                        left: t.right,
                        top: t.top,
                        right: big.right,
                        bottom: t.bottom,
                    },
                ] {
                    if r.right > r.left && r.bottom > r.top {
                        fill_rect(ctx, rf(r), ui::SCRIM);
                    }
                }
            }
            None => fill_rect(ctx, rf(big), ui::SCRIM),
        }
        let Some(t) = target else { return };
        // 親の UI 要素の範囲 (破線)。
        let showing_element = o.drag.is_none()
            && o.chain_window == o.hover
            && o.level < o.chain.len().saturating_sub(1);
        if showing_element && let Some(parent) = o.chain.get(o.level + 1) {
            dashed_rect(ctx, parent.rect, s);
        }
        highlight(ctx, t, s);
        if let Some(d) = &o.drag {
            let motion = anim::motion();
            let mut fade = o.snap_fade.borrow_mut();
            let snaps: Vec<&Snapped> = d.anchor_snaps.iter().chain(&d.snaps).collect();
            fade.retain(|k| snaps.iter().any(|sn| (sn.vertical, sn.edge.pos) == *k));
            for sn in snaps {
                let a = fade.enter((sn.vertical, sn.edge.pos), 1.0, anim::SHOW_MS, motion, 0.0);
                with_opacity(ctx, a, |c| snapped_edge(c, t, sn, s));
            }
            if fade.busy() {
                SetTimer(Some(m.hwnd), TIMER_SNAP, 15, None);
            }
        }
        // ラベル。
        let core = stroke(2.0, s) + stroke(1.0, s);
        let outer = inflate(t, core);
        let gap = px(8.0, s);
        let dims = ui::text(
            &format!("{} × {}", t.right - t.left, t.bottom - t.top),
            Style::NumStrong,
            s,
        );
        let (label, side) = if o.drag.is_some() {
            (
                Label::new(vec![Part::Text(dims, pal.text_primary)], s),
                Side::Below,
            )
        } else if showing_element {
            let e = &o.chain[o.level];
            let title = if e.name.is_empty() {
                e.kind.clone()
            } else if e.kind.is_empty() {
                e.name.clone()
            } else {
                format!("{} · {}", e.kind, e.name)
            };
            let parts = vec![
                Part::Text(
                    ui::text(&clip(&title), Style::Caption, s),
                    pal.text_secondary,
                ),
                Part::Gap(gap as f32),
                Part::Text(dims, pal.text_primary),
                Part::Gap(gap as f32),
                Part::Icon("mouse", pal.text_secondary),
                Part::Gap(px(4.0, s) as f32),
                Part::Text(
                    ui::text(strings.wheel_to_parent, Style::Caption, s),
                    pal.text_secondary,
                ),
            ];
            (Label::new(parts, s), Side::AboveThenBelow)
        } else {
            let title = o
                .hover
                .map(|i| o.windows[i].title.clone())
                .unwrap_or_default();
            let mut parts = Vec::new();
            if !title.is_empty() {
                parts.push(Part::Text(
                    ui::text(&clip(&title), Style::Caption, s),
                    pal.text_secondary,
                ));
                parts.push(Part::Gap(gap as f32));
            }
            parts.push(Part::Text(dims, pal.text_primary));
            (Label::new(parts, s), Side::Above)
        };
        // モニタに依らない点で置き場を決める。2 モニタ跨ぎでもラベルは 1 か所で、各モニタは自分の部分だけ描く。
        // 画面の外へはみ出た対象 (最大化の -8 など) は仮想画面の中へ寄せ、先頭のモニタへの取り違えを避ける。
        let v = o.virt().unwrap_or(m.rect);
        let anchor = POINT {
            x: t.left.clamp(v.left, v.right - 1),
            y: t.top.clamp(v.top, v.bottom - 1),
        };
        let p = place_label(outer, t, label.w, label.h, gap, side, o.monitor_at(anchor));
        label.draw(ctx, p.x as f32, p.y as f32, pal, s);
    });
}

/// 長い題名はラベルが画面を横切らないよう切る。
fn clip(t: &str) -> String {
    const MAX: usize = 48;
    if t.chars().count() > MAX {
        let mut s: String = t.chars().take(MAX - 1).collect();
        s.push('…');
        s
    } else {
        t.to_string()
    }
}

/// ハイライト: 内から芯 2 (`hl-core`)・縁 1 (`hl-rim`)、外ににじみ (`shadow-hl` 0 0 14 3 .42)。
pub fn highlight(ctx: &ID2D1DeviceContext, t: RECT, s: f32) {
    let core = stroke(2.0, s);
    let rim = stroke(1.0, s);
    let spread = px(3.0, s);
    ui::shadow(
        ctx,
        7.0 * s,
        ui::rgba(0, 0, 0, 0.42),
        (0.0, 0.0),
        Some(rf(t)),
        |c| {
            fill_rect(c, rf(inflate(t, core + rim + spread)), ui::HL_DARK);
        },
    );
    let half = |w: i32| w as f32 / 2.0;
    let r = rf(t);
    let grow = |d: f32| rectf(r.left - d, r.top - d, r.right + d, r.bottom + d);
    stroke_rect(ctx, grow(half(core)), core as f32, ui::HL_CORE);
    stroke_rect(ctx, grow(core as f32 + half(rim)), rim as f32, ui::HL_RIM);
}

fn dash_style(on: f32, off: f32, offset: f32) -> Option<ID2D1StrokeStyle> {
    unsafe {
        ui::gfx()
            .factory
            .CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES1 {
                    dashStyle: D2D1_DASH_STYLE_CUSTOM,
                    dashOffset: offset,
                    ..Default::default()
                },
                Some(&[on, off]),
            )
            .ok()
            .and_then(|s| windows::core::Interface::cast(&s).ok())
    }
}

/// 親の範囲: 外側 1 に、白と黒を 4 ずつ交互の 1 の破線。
fn dashed_rect(ctx: &ID2D1DeviceContext, r: RECT, s: f32) {
    let w = stroke(1.0, s) as f32;
    let seg = 4.0 * s / w;
    let rr = rf(r);
    let line = rectf(
        rr.left - w / 2.0,
        rr.top - w / 2.0,
        rr.right + w / 2.0,
        rr.bottom + w / 2.0,
    );
    unsafe {
        let black = ctx
            .CreateSolidColorBrush(&ui::HL_DARK, None)
            .expect("brush");
        let white = ctx
            .CreateSolidColorBrush(&ui::HL_CORE, None)
            .expect("brush");
        let style = dash_style(seg, seg, 0.0);
        ctx.DrawRectangle(&line, &black, w, None);
        ctx.DrawRectangle(&line, &white, w, style.as_ref());
    }
}

/// 吸着した辺 (幅 4、長さは辺 + 6、矩形の外 3 から、角丸 2、`shadow-snap-edge`) と、
/// 吸着先の辺の線上で矩形の外に出ている所の案内線 (2、白 6 / 空き 4、`shadow-snap`)。
fn snapped_edge(ctx: &ID2D1DeviceContext, t: RECT, sn: &Snapped, s: f32) {
    let off = px(3.0, s) as f32;
    let bar = px(4.0, s) as f32;
    let r = rf(t);
    let e = sn.edge.pos as f32;
    let bar_rect = if sn.vertical {
        let x0 = if sn.edge.pos == t.left {
            e - off - bar
        } else {
            e + off
        };
        rectf(x0, r.top - off, x0 + bar, r.bottom + off)
    } else {
        let y0 = if sn.edge.pos == t.top {
            e - off - bar
        } else {
            e + off
        };
        rectf(r.left - off, y0, r.right + off, y0 + bar)
    };
    let radius = px(2.0, s) as f32;
    ui::shadow(
        ctx,
        4.0 * s,
        ui::rgba(0, 0, 0, 0.5),
        (0.0, 0.0),
        None,
        |c| {
            ui::fill_round(c, bar_rect, radius, ui::HL_DARK);
        },
    );
    ui::fill_round(ctx, bar_rect, radius, ui::HL_CORE);

    // 案内線: 辺の範囲のうち、矩形の外に出ている所。
    let lw = stroke(2.0, s) as f32;
    let (lo, hi) = if sn.vertical {
        (t.top, t.bottom)
    } else {
        (t.left, t.right)
    };
    let segs = [
        (sn.edge.from, lo.min(sn.edge.to)),
        (hi.max(sn.edge.from), sn.edge.to),
    ];
    let style = dash_style(6.0 * s / lw, 4.0 * s / lw, 0.0);
    let draw = |c: &ID2D1DeviceContext, color: D2D1_COLOR_F| unsafe {
        let brush = c.CreateSolidColorBrush(&color, None).expect("brush");
        for (a, b) in segs {
            if b <= a {
                continue;
            }
            let (p0, p1) = if sn.vertical {
                let x = if sn.edge.pos == t.left {
                    e - lw / 2.0
                } else {
                    e + lw / 2.0
                };
                ((x, a as f32), (x, b as f32))
            } else {
                let y = if sn.edge.pos == t.top {
                    e - lw / 2.0
                } else {
                    e + lw / 2.0
                };
                ((a as f32, y), (b as f32, y))
            };
            c.DrawLine(
                windows_numerics::Vector2 { X: p0.0, Y: p0.1 },
                windows_numerics::Vector2 { X: p1.0, Y: p1.1 },
                &brush,
                lw,
                style.as_ref(),
            );
        }
    };
    ui::shadow(
        ctx,
        3.0 * s,
        ui::rgba(0, 0, 0, 0.55),
        (0.0, 0.0),
        None,
        |c| draw(c, ui::HL_DARK),
    );
    draw(ctx, ui::HL_CORE);
}

/// `f` で描く物を不透明度 `a` で重ねる。
fn with_opacity(ctx: &ID2D1DeviceContext, a: f32, f: impl FnOnce(&ID2D1DeviceContext)) {
    if a >= 1.0 {
        return f(ctx);
    }
    unsafe {
        ctx.PushLayer(
            &D2D1_LAYER_PARAMETERS1 {
                contentBounds: rectf(f32::MIN, f32::MIN, f32::MAX, f32::MAX),
                maskTransform: windows_numerics::Matrix3x2::identity(),
                opacity: a,
                ..Default::default()
            },
            None,
        );
        f(ctx);
        ctx.PopLayer();
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_SETCURSOR => {
            unsafe {
                SetCursor(LoadCursorW(None, IDC_CROSS).ok());
            }
            LRESULT(1)
        }
        WM_MOUSEMOVE => {
            update_hover();
            redraw_all();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_SNAP => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_SNAP);
            }
            redraw_all();
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            unsafe {
                SetCapture(hwnd);
            }
            OVERLAY.with(|o| {
                if let Some(o) = o.borrow_mut().as_mut() {
                    o.press = Some(cursor());
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            // ReleaseCapture は WM_CAPTURECHANGED を同期で送り、ドラッグを捨てる。矩形はその前に取る。
            let rect = OVERLAY
                .with(|o| o.borrow().as_ref().and_then(|o| o.target()))
                .filter(|r| r.right - r.left > 0 && r.bottom - r.top > 0);
            unsafe {
                let _ = ReleaseCapture();
            }
            if rect.is_some() {
                finish(rect);
            } else {
                // Ctrl なしのクリック (ドラッグしていない) か面積 0: 何も選ばず、選択を続ける。
                OVERLAY.with(|o| {
                    if let Some(o) = o.borrow_mut().as_mut() {
                        o.press = None;
                        o.drag = None;
                    }
                });
                redraw_all();
            }
            LRESULT(0)
        }
        // ドラッグ中にキャプチャを奪われた (Win キー・Alt+Tab): 押下とドラッグを捨てる。確定はしない。
        WM_CAPTURECHANGED => {
            OVERLAY.with(|o| {
                if let Some(o) = o.borrow_mut().as_mut() {
                    o.press = None;
                    o.drag = None;
                    *o.snap_fade.borrow_mut() = Anim::default();
                }
            });
            redraw_all();
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            finish(None);
            LRESULT(0)
        }
        // Alt を押している間のキーは WM_SYSKEY* で届く (Ctrl+Alt+Shift+S のような Alt を含むホットキー)。
        WM_KEYDOWN | WM_SYSKEYDOWN if wparam.0 as u16 == VK_ESCAPE.0 => {
            finish(None);
            LRESULT(0)
        }
        // 押しっぱなしの自動の繰り返し (lparam の bit 30) は捨てる。
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP
            if wparam.0 as u16 == VK_CONTROL.0
                && (matches!(msg, WM_KEYUP | WM_SYSKEYUP) || lparam.0 & (1 << 30) == 0) =>
        {
            update_hover();
            redraw_all();
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = (wparam.0 >> 16) as u16 as i16;
            let changed = OVERLAY.with(|o| {
                let mut o = o.borrow_mut();
                let Some(o) = o.as_mut() else { return false };
                if o.drag.is_some() || o.chain_window != o.hover || o.chain.is_empty() {
                    return false;
                }
                let top = o.chain.len() - 1;
                let before = o.level;
                // ↑ で親、↓ で子。
                o.level = if delta > 0 {
                    (o.level + 1).min(top)
                } else {
                    o.level.saturating_sub(1)
                };
                o.level != before
            });
            if changed {
                redraw_all();
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // 窓はモニタの矩形のまま (推奨矩形は使わない)。線とラベルの s だけを替える。
            OVERLAY.with(|o| {
                if let Some(o) = o.borrow_mut().as_mut()
                    && let Some(m) = o.mons.iter_mut().find(|m| m.hwnd == hwnd)
                {
                    m.s = (wparam.0 & 0xFFFF) as f32 / 96.0;
                }
            });
            redraw_all();
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

    fn el(l: i32, t: i32, r: i32, b: i32) -> uia::Element {
        uia::Element {
            rect: RECT {
                left: l,
                top: t,
                right: r,
                bottom: b,
            },
            kind: String::new(),
            name: String::new(),
        }
    }

    #[test]
    fn trim_chain_drops_elements_the_point_left() {
        // 深い順: ボタン、ツールバー、窓。
        let chain = || vec![el(10, 10, 20, 20), el(0, 0, 100, 30), el(0, 0, 500, 500)];
        let mut c = chain();
        // ボタンの中: 何も落とさない。
        assert_eq!(trim_chain(&mut c, POINT { x: 15, y: 15 }), 0);
        // ツールバーの中だがボタンの外: ボタンだけ落とす。
        assert_eq!(trim_chain(&mut c, POINT { x: 50, y: 15 }), 1);
        assert_eq!(c[0].rect.right, 100);
        // 窓の外まで出ても窓は残す。
        let mut c = chain();
        assert_eq!(trim_chain(&mut c, POINT { x: 900, y: 900 }), 2);
        assert_eq!(c.len(), 1);
    }
}
