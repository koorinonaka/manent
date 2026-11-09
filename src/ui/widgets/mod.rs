//! 部品キット: acrylic の窓、ツールのボタン、主の文字のボタン、区切り、ツールバー、ツールチップ、
//! スライダー、フォーカス表示、D2D 半透明のラベル、アイコン (`assets/icons.json`)。
//! 寸法は DS「Odile」の Toolbar・Buttons・Focus・Surfaces の表 (100% の px) を `px` で物理 px にする。
//! 後続 (7929・a6ed・c0d4) もここに部品を足す。タイムライン (GIF ピンの帯とフィルムストリップ) は `timeline`。

pub mod status;
pub mod timeline;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HSTRING, PCWSTR, Result};

use super::{
    Palette, Style, Surface, Text, anim, draw_text, fill_rect, fill_round, gfx, px, rectf, stroke,
};

// ---- アイコン ----

const ICONS_JSON: &str = include_str!("../../../assets/icons.json");

#[derive(Debug, Clone, Copy, PartialEq)]
enum Seg {
    Move(f32, f32),
    Line(f32, f32),
    Cubic(f32, f32, f32, f32, f32, f32),
    /// rx, ry, 回転, 大きい弧, 時計回り, x, y
    Arc(f32, f32, f32, bool, bool, f32, f32),
    Close,
}

/// SVG のパス (`M L H V A C Z` の絶対座標だけ。DS の icons.json と Mark の SVG の書き方) を読む。
fn parse_path(d: &str) -> Vec<Seg> {
    let mut out = Vec::new();
    let bytes = d.as_bytes();
    let mut i = 0;
    let mut cmd = b'M';
    let mut cur = (0.0f32, 0.0f32);
    let mut nums: Vec<f32> = Vec::new();
    let flush = |cmd: u8, nums: &mut Vec<f32>, cur: &mut (f32, f32), out: &mut Vec<Seg>| {
        let n = nums.as_slice();
        match cmd {
            b'M' => {
                for (k, p) in n.chunks_exact(2).enumerate() {
                    *cur = (p[0], p[1]);
                    out.push(if k == 0 {
                        Seg::Move(p[0], p[1])
                    } else {
                        Seg::Line(p[0], p[1])
                    });
                }
            }
            b'L' => n.chunks_exact(2).for_each(|p| {
                *cur = (p[0], p[1]);
                out.push(Seg::Line(p[0], p[1]));
            }),
            b'H' => n.iter().for_each(|&x| {
                cur.0 = x;
                out.push(Seg::Line(cur.0, cur.1));
            }),
            b'V' => n.iter().for_each(|&y| {
                cur.1 = y;
                out.push(Seg::Line(cur.0, cur.1));
            }),
            b'C' => n.chunks_exact(6).for_each(|p| {
                *cur = (p[4], p[5]);
                out.push(Seg::Cubic(p[0], p[1], p[2], p[3], p[4], p[5]));
            }),
            b'A' => n.chunks_exact(7).for_each(|p| {
                *cur = (p[5], p[6]);
                out.push(Seg::Arc(
                    p[0],
                    p[1],
                    p[2],
                    p[3] != 0.0,
                    p[4] != 0.0,
                    p[5],
                    p[6],
                ));
            }),
            b'Z' => out.push(Seg::Close),
            _ => {}
        }
        nums.clear();
    };
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_alphabetic() {
            flush(cmd, &mut nums, &mut cur, &mut out);
            cmd = c.to_ascii_uppercase();
            if cmd == b'Z' {
                flush(cmd, &mut nums, &mut cur, &mut out);
                cmd = b'_';
            }
            i += 1;
        } else if c == b'-' || c == b'.' || c.is_ascii_digit() {
            let start = i;
            i += 1;
            let mut dot = c == b'.';
            while i < bytes.len() {
                let b = bytes[i];
                if b.is_ascii_digit() || (b == b'.' && !dot) {
                    dot |= b == b'.';
                    i += 1;
                } else {
                    break;
                }
            }
            nums.push(d[start..i].parse().unwrap_or(0.0));
        } else {
            i += 1;
        }
    }
    flush(cmd, &mut nums, &mut cur, &mut out);
    out
}

pub fn geometry(d: &str) -> Result<ID2D1PathGeometry1> {
    let v2 = |x: f32, y: f32| windows_numerics::Vector2 { X: x, Y: y };
    unsafe {
        let geo = gfx().factory.CreatePathGeometry()?;
        let sink = geo.Open()?;
        let mut open = false;
        for seg in parse_path(d) {
            match seg {
                Seg::Move(x, y) => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_OPEN);
                    }
                    sink.BeginFigure(v2(x, y), D2D1_FIGURE_BEGIN_FILLED);
                    open = true;
                }
                Seg::Line(x, y) => sink.AddLine(v2(x, y)),
                Seg::Cubic(a, b, c, d, e, f) => sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: v2(a, b),
                    point2: v2(c, d),
                    point3: v2(e, f),
                }),
                Seg::Arc(rx, ry, rot, large, sweep, x, y) => sink.AddArc(&D2D1_ARC_SEGMENT {
                    point: v2(x, y),
                    size: D2D_SIZE_F {
                        width: rx,
                        height: ry,
                    },
                    rotationAngle: rot,
                    sweepDirection: if sweep {
                        D2D1_SWEEP_DIRECTION_CLOCKWISE
                    } else {
                        D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE
                    },
                    arcSize: if large {
                        D2D1_ARC_SIZE_LARGE
                    } else {
                        D2D1_ARC_SIZE_SMALL
                    },
                }),
                Seg::Close => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                        open = false;
                    }
                }
            }
        }
        if open {
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
        }
        sink.Close()?;
        Ok(geo)
    }
}

/// icons.json の 1 個 (`stroke` と `fill` の文字列の配列) を読む。JSON の書き方は DS の出力に固定。
fn icon_paths(id: &str) -> (Vec<&'static str>, Vec<&'static str>) {
    let key = format!("\"id\": \"{id}\"");
    let Some(at) = ICONS_JSON.find(&key) else {
        eprintln!("icon not found: {id}");
        return (Vec::new(), Vec::new());
    };
    let rest = &ICONS_JSON[at..];
    let list = |name: &str| -> Vec<&'static str> {
        let Some(open) = rest.find(&format!("\"{name}\": [")) else {
            return Vec::new();
        };
        let body = &rest[open..];
        let body = &body[body.find('[').unwrap() + 1..body.find(']').unwrap()];
        body.split('"').skip(1).step_by(2).collect()
    };
    (list("stroke"), list("fill"))
}

struct Icon {
    strokes: Vec<ID2D1PathGeometry1>,
    fills: Vec<ID2D1PathGeometry1>,
}

thread_local! {
    static ICONS: RefCell<HashMap<&'static str, Icon>> = RefCell::new(HashMap::new());
    static ROUND: RefCell<Option<ID2D1StrokeStyle1>> = const { RefCell::new(None) };
}

pub fn round_stroke() -> ID2D1StrokeStyle1 {
    ROUND.with(|r| {
        r.borrow_mut()
            .get_or_insert_with(|| unsafe {
                gfx()
                    .factory
                    .CreateStrokeStyle(
                        &D2D1_STROKE_STYLE_PROPERTIES1 {
                            startCap: D2D1_CAP_STYLE_ROUND,
                            endCap: D2D1_CAP_STYLE_ROUND,
                            dashCap: D2D1_CAP_STYLE_ROUND,
                            lineJoin: D2D1_LINE_JOIN_ROUND,
                            ..Default::default()
                        },
                        None,
                    )
                    .expect("stroke style")
            })
            .clone()
    })
}

/// 18 の格子のアイコンを (x, y) を左上に、辺 `size` (物理 px) で描く。線は 1.6 × size/18。
pub fn draw_icon(
    ctx: &ID2D1DeviceContext,
    id: &'static str,
    x: f32,
    y: f32,
    size: f32,
    color: D2D1_COLOR_F,
) {
    ICONS.with(|icons| {
        let mut icons = icons.borrow_mut();
        let icon = icons.entry(id).or_insert_with(|| {
            let (st, fi) = icon_paths(id);
            Icon {
                strokes: st.iter().filter_map(|d| geometry(d).ok()).collect(),
                fills: fi.iter().filter_map(|d| geometry(d).ok()).collect(),
            }
        });
        let k = size / 18.0;
        unsafe {
            let mut old = super::identity();
            ctx.GetTransform(&mut old);
            let m = windows_numerics::Matrix3x2::scale(k, k)
                * windows_numerics::Matrix3x2::translation(x, y)
                * old;
            ctx.SetTransform(&m);
            let brush = ctx.CreateSolidColorBrush(&color, None).expect("brush");
            let style = round_stroke();
            for g in &icon.fills {
                ctx.FillGeometry(g, &brush, None);
            }
            for g in &icon.strokes {
                ctx.DrawGeometry(g, &brush, 1.6, &style);
            }
            ctx.SetTransform(&old);
        }
    });
}

// ---- 窓 ----

/// アプリの窓クラス。アイコンは exe の資源 ID 1 (`assets/app-icon.ico`) を全部の窓が持つ。
pub fn register_class(
    name: PCWSTR,
    wndproc: unsafe extern "system" fn(
        HWND,
        u32,
        windows::Win32::Foundation::WPARAM,
        windows::Win32::Foundation::LPARAM,
    ) -> windows::Win32::Foundation::LRESULT,
) {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let icon = |cx, cy| {
            LoadImageW(
                Some(instance.into()),
                PCWSTR(1 as _),
                IMAGE_ICON,
                cx,
                cy,
                LR_DEFAULTCOLOR,
            )
            .map(|h| HICON(h.0))
            .unwrap_or_default()
        };
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: name,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: icon(GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON)),
            hIconSm: icon(GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

pub const ACCENT_ENABLE_BLURBEHIND: u32 = 3;
#[cfg(debug_assertions)]
pub const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;

/// 窓の背後をぼかして透かす (liveback の `blur_window_backdrop` と同じ形)。
/// undocumented の `user32!SetWindowCompositionAttribute` (`WCA_ACCENT_POLICY` = 19) を実行時に引く。
/// `DWMWA_SYSTEMBACKDROP_TYPE` は DComp の swap chain の下に不透明な単色 (84,84,84) を敷くだけで、背後は透けない
/// (t261104-ca82 の手順 1、KNOWLEDGE「ガラス HUD と凍結の実測」)。`gradient_abgr` は accent の色 (地は swap chain が塗る)。
/// export が無い・呼び出しが拒まれたら 1 行出して `false`。
pub fn set_accent(hwnd: HWND, state: u32, gradient_abgr: u32) -> bool {
    use windows::Win32::System::LibraryLoader::GetProcAddress;
    #[repr(C)]
    struct AccentPolicy {
        state: u32,
        flags: u32,
        gradient_abgr: u32,
        animation: u32,
    }
    #[repr(C)]
    struct CompositionAttribute {
        attribute: u32,
        data: *mut std::ffi::c_void,
        size: usize,
    }
    type SetWindowCompositionAttribute =
        unsafe extern "system" fn(HWND, *mut CompositionAttribute) -> BOOL;
    const WCA_ACCENT_POLICY: u32 = 19;
    let Ok(user32) = (unsafe { GetModuleHandleW(windows::core::w!("user32.dll")) }) else {
        eprintln!("blur behind: user32 not loaded");
        return false;
    };
    let Some(address) =
        (unsafe { GetProcAddress(user32, windows::core::s!("SetWindowCompositionAttribute")) })
    else {
        eprintln!("blur behind: SetWindowCompositionAttribute not exported");
        return false;
    };
    // SAFETY: Windows 7 からこの形。
    let set: SetWindowCompositionAttribute = unsafe { std::mem::transmute(address) };
    let mut policy = AccentPolicy {
        state,
        flags: 0,
        gradient_abgr,
        animation: 0,
    };
    let mut attribute = CompositionAttribute {
        attribute: WCA_ACCENT_POLICY,
        data: std::ptr::from_mut(&mut policy).cast(),
        size: size_of::<AccentPolicy>(),
    };
    let ok = unsafe { set(hwnd, &mut attribute) }.as_bool();
    if !ok {
        eprintln!("blur behind: refused (state {state})");
    }
    ok
}

const MARGIN_TIMER: usize = 0xCA82;

/// ピンの影の余白のクリックを下の窓へ通す。`WM_NCHITTEST` で点が余白にあるときに呼ぶ。
/// `HTTRANSPARENT` は同じスレッドの窓にしか通らない (t261104-ca82: 別プロセスの窓へは 5 回中 0 回)。
/// だから窓に `WS_EX_LAYERED | WS_EX_TRANSPARENT` を足す (DComp の窓は描画を続ける。KNOWLEDGE)。
/// 透過中は窓にマウスのメッセージが来ないので、タイマーでカーソルを見て、窓に `WM_NCHITTEST` を送り直し、
/// 余白でなくなったか窓の外へ出たら外す。クリック透過モードに入るときは `stop_margin_pass` を呼ぶ。
// ponytail: 30 ms の見回り。余白から画像へ入った直後 30 ms 以内のクリックは下へ抜ける。
pub fn pass_margin(hwnd: HWND) {
    let bits = (WS_EX_LAYERED | WS_EX_TRANSPARENT).0 as isize;
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if ex & bits != bits {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | bits);
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
        }
        SetTimer(Some(hwnd), MARGIN_TIMER, 30, Some(margin_tick));
    }
}

/// `pass_margin` の見回りを止める (付けた透過のビットはそのまま)。
pub fn stop_margin_pass(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), MARGIN_TIMER);
    }
}

unsafe extern "system" fn margin_tick(hwnd: HWND, _: u32, id: usize, _: u32) {
    let pt = super::cursor();
    let mut r = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut r);
        let inside = pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom;
        let lparam = LPARAM(((pt.y as u16 as isize) << 16) | pt.x as u16 as isize);
        // 余白にいる間は wndproc が `pass_margin` を呼び直す (タイマーも張り直る)。
        if inside
            && SendMessageW(hwnd, WM_NCHITTEST, None, Some(lparam)).0 == HTTRANSPARENT as isize
        {
            return;
        }
        let _ = KillTimer(Some(hwnd), id);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let bits = (WS_EX_LAYERED | WS_EX_TRANSPARENT).0 as isize;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !bits);
        if inside {
            // 透過の間の移動は下の窓へ行った。止まったカーソルでもホバー (HUD) が出るよう、移動を 1 つ渡す。
            let client =
                LPARAM((((pt.y - r.top) as u16 as isize) << 16) | (pt.x - r.left) as u16 as isize);
            let _ = PostMessageW(Some(hwnd), WM_MOUSEMOVE, WPARAM(0), client);
        }
    }
}

/// 前面を取らない HUD の別窓 (ツールバー・ツールチップ・ポップアップ)。
/// `Acrylic` の窓をほかのキャプチャに写すか。
#[derive(Clone, Copy)]
pub enum Capture {
    /// 常に写す (設定窓)。
    Shown,
    /// 設定 `show_in_capture` に従う (ピン・GIF ピンの HUD、トレイメニュー)。
    Setting,
    /// 常に写さない (録画のバー・カウントダウン)。
    Never,
}

/// 地は背後のぼかし (accent の BLURBEHIND) の上に `surface-acrylic` を塗る。accent が付かない窓は `surface-opaque`。
/// `DWMWA_SYSTEMBACKDROP_TYPE` は使わない: NOACTIVATE の窓では DWM の単色 (84) になる (KNOWLEDGE「ガラス HUD と凍結の実測」)。
/// debug ビルドは `ODILE_NO_BLUR=1` で accent を付けない (コストの A/B 用)。
pub struct Acrylic {
    pub hwnd: HWND,
    pub surface: Surface,
    /// 背後のぼかし (accent) を付けられた。
    pub blur: bool,
    /// 出している。
    shown: Cell<bool>,
    topmost: Cell<bool>,
}

impl Acrylic {
    /// `class` は `register_class` 済みのクラス。`small` はツールチップの角 (`DWMWCP_ROUNDSMALL`)。
    /// `capture` はほかのキャプチャに写すか。設定窓の一覧は撮れるように `Capture::Shown`。
    pub fn new(class: PCWSTR, small: bool, capture: Capture) -> Result<Self> {
        unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP,
                class,
                &HSTRING::new(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                GetModuleHandleW(None).ok().map(Into::into),
                None,
            )?;
            let _ = DwmExtendFrameIntoClientArea(
                hwnd,
                &MARGINS {
                    cxLeftWidth: -1,
                    cxRightWidth: -1,
                    cyTopHeight: -1,
                    cyBottomHeight: -1,
                },
            );
            let blur = !(cfg!(debug_assertions) && std::env::var_os("ODILE_NO_BLUR").is_some())
                && set_accent(hwnd, ACCENT_ENABLE_BLURBEHIND, 0x0100_0000);
            let corner = if small {
                DWMWCP_ROUNDSMALL
            } else {
                DWMWCP_ROUND
            };
            let _ = set_attr(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner);
            // DWM の出入りの遷移は残す: 背後のぼかしは DComp の不透明度が効かず、DWM の遷移でしか薄れない。
            // 切ると `hide` の最後でぼかしだけが一度に消える。
            match capture {
                Capture::Shown => {}
                Capture::Setting => super::exclude_from_capture(hwnd),
                Capture::Never => super::always_exclude_from_capture(hwnd),
            }
            let surface = Surface::new(hwnd, 1, 1)?;
            let this = Self {
                hwnd,
                surface,
                blur,
                shown: Cell::new(false),
                topmost: Cell::new(true),
            };
            this.set_theme(super::apps_dark());
            Ok(this)
        }
    }

    pub fn set_theme(&self, dark: bool) {
        let _ = set_attr(self.hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &BOOL::from(dark));
    }

    /// 地を塗ってから `f` で中身を描く。ぼかしが付いていれば `surface-acrylic`、無ければ `surface-opaque`。
    pub fn paint(&self, pal: &Palette, f: impl FnOnce(&ID2D1DeviceContext)) {
        let ground = if self.blur {
            pal.surface_acrylic
        } else {
            pal.surface_opaque
        };
        let (w, h) = (self.surface.w as f32, self.surface.h as f32);
        self.surface.draw(|ctx| {
            fill_rect(ctx, rectf(0.0, 0.0, w, h), ground);
            f(ctx);
        });
    }

    /// 物理 px の矩形へ動かして出す (前面は取らない)。隠れていたらフェードで出す。
    pub fn show_at(&mut self, r: RECT) {
        let (w, h) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
        let _ = self.surface.resize(w, h);
        // ゆっくり消す途中なら、すぐ濃く戻す。
        self.set_alpha(1.0);
        // 不透明度 0 を先に付けてから出す (出した最初のコマが濃く出ない)。
        if !self.shown.replace(true) {
            let ms = if anim::motion() { anim::SHOW_MS } else { 0.0 };
            self.surface.fade(0.0, 1.0, ms);
        }
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(if self.topmost.get() {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                }),
                r.left,
                r.top,
                w as i32,
                h as i32,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }

    /// フェードを待たずに今のコマで隠す (持ち主のピンを閉じたとき)。
    pub fn hide_now(&self) {
        self.shown.set(false);
        unsafe {
            // このコマで消す: DWM の遷移もこの 1 回だけ切る。
            let _ = set_attr(
                self.hwnd,
                DWMWA_TRANSITIONS_FORCEDISABLED,
                &BOOL::from(true),
            );
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            let _ = set_attr(
                self.hwnd,
                DWMWA_TRANSITIONS_FORCEDISABLED,
                &BOOL::from(false),
            );
        }
    }

    /// 最前面に置くか (既定は置く)。持ち主のピンの最前面に合わせる。
    pub fn set_topmost(&self, on: bool) {
        self.topmost.set(on);
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(if on { HWND_TOPMOST } else { HWND_NOTOPMOST }),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    /// 窓ごとの不透明度 (`LWA_ALPHA`)。背後のぼかしも一緒に薄れる (DComp の不透明度はぼかしに効かない)。
    /// 1 で外す。ゆっくり消すとき、持ち主が画面の更新ごとに下げ、0 で `hide_now` する。
    pub fn set_alpha(&self, a: f32) {
        unsafe {
            let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            let layered = ex & WS_EX_LAYERED.0 as isize != 0;
            if a >= 1.0 {
                if layered {
                    SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex & !(WS_EX_LAYERED.0 as isize));
                }
                return;
            }
            if !layered {
                SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as isize);
            }
            let a = (a.max(0.0) * 255.0).round() as u8;
            let _ = SetLayeredWindowAttributes(self.hwnd, COLORREF(0), a, LWA_ALPHA);
        }
    }

    /// 隠す。消え方は DWM の遷移に任せる: ボタン・地・背後のぼかしが 1 回のフェードで一緒に消える
    /// (DComp で先に薄めると、ぼかしだけ後に残って 2 段になる)。
    pub fn hide(&self) {
        if !self.shown.replace(false) {
            return;
        }
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    /// 出している。
    pub fn visible(&self) -> bool {
        self.shown.get()
    }

    /// 画面に見えている (`set_alpha` で薄めている途中も真)。
    pub fn on_screen(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd).as_bool() }
    }
}

pub fn set_attr<T>(hwnd: HWND, attr: DWMWINDOWATTRIBUTE, value: &T) -> Result<()> {
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            attr,
            (value as *const T).cast(),
            size_of::<T>() as u32,
        )
    }
}

// ---- ボタン ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BtnState {
    Normal,
    Hover,
    Pressed,
    /// ポップアップを開いている。
    Open,
    On,
    OnHover,
    OnPressed,
    Disabled,
}

/// ボタンの見た目の度合い (各 0..=1)。`BtnState` の途中の色を描くのに使う。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Look {
    pub hover: f32,
    pub press: f32,
    pub on: f32,
    pub disabled: bool,
}

impl From<BtnState> for Look {
    fn from(b: BtnState) -> Self {
        let (hover, press, on) = match b {
            BtnState::Normal | BtnState::Disabled => (0.0, 0.0, 0.0),
            BtnState::Hover => (1.0, 0.0, 0.0),
            BtnState::Pressed | BtnState::Open => (1.0, 1.0, 0.0),
            BtnState::On => (0.0, 0.0, 1.0),
            BtnState::OnHover => (1.0, 0.0, 1.0),
            BtnState::OnPressed => (1.0, 1.0, 1.0),
        };
        Self {
            hover,
            press,
            on,
            disabled: b == BtnState::Disabled,
        }
    }
}

impl Look {
    /// `target` へ寄せた途中の見た目。`key` はボタンの添字。ホバーと押下とオンで速さが違う。
    pub fn eased(
        target: BtnState,
        key: usize,
        anims: &mut anim::Anim<(usize, u8)>,
        motion: bool,
    ) -> Self {
        let t = Look::from(target);
        Self {
            hover: anims.to((key, 0), t.hover, anim::HOVER_MS, motion),
            press: anims.to((key, 1), t.press, anim::PRESS_MS, motion),
            on: anims.to((key, 2), t.on, anim::ON_MS, motion),
            disabled: t.disabled,
        }
    }
}

/// ツールのボタン (32 × 32、アイコン 18 を中央、角丸 4)。(x, y) は外形の左上。
pub fn tool_button(
    ctx: &ID2D1DeviceContext,
    x: f32,
    y: f32,
    icon: &'static str,
    look: impl Into<Look>,
    pal: &Palette,
    s: f32,
) {
    icon_button(ctx, x, y, 32.0, 18.0, icon, look.into(), pal, s);
}

/// トレイのアイコンのボタン (22 × 22、アイコン 16 を中央、角丸 4)。状態の色はツールのボタンと同じ。
pub fn tray_icon_button(
    ctx: &ID2D1DeviceContext,
    x: f32,
    y: f32,
    icon: &'static str,
    look: impl Into<Look>,
    pal: &Palette,
    s: f32,
) {
    icon_button(ctx, x, y, 22.0, 16.0, icon, look.into(), pal, s);
}

/// 文字のボタン (`Item::Wide`: 56 × 32、角丸 4、文字は `num-strong` を中央)。状態の色はツールのボタンと同じ。
pub fn label_button(
    ctx: &ID2D1DeviceContext,
    x: f32,
    y: f32,
    label: &str,
    look: impl Into<Look>,
    pal: &Palette,
    s: f32,
) {
    let (w, h) = (px(WIDE_W, s) as f32, px(32.0, s) as f32);
    let ink = button_fill(ctx, rectf(x, y, x + w, y + h), look.into(), pal, s);
    let t = super::text(label, Style::NumStrong, s);
    draw_text(
        ctx,
        &t,
        (x + (w - t.w) / 2.0).round(),
        (y + (h - t.h) / 2.0).round(),
        ink,
    );
}

/// アイコンだけのボタン。`size` と `icon` は 100% の px。
#[allow(clippy::too_many_arguments)]
fn icon_button(
    ctx: &ID2D1DeviceContext,
    x: f32,
    y: f32,
    size: f32,
    icon_px: f32,
    icon: &'static str,
    look: Look,
    pal: &Palette,
    s: f32,
) {
    let size = px(size, s) as f32;
    let ink = button_fill(ctx, rectf(x, y, x + size, y + size), look, pal, s);
    let icon_size = icon_px * s;
    let off = (size - icon_size) / 2.0;
    draw_icon(ctx, icon, x + off, y + off, icon_size, ink);
}

/// ボタンの地 (角丸 4) を塗り、中身の色を返す。
fn button_fill(
    ctx: &ID2D1DeviceContext,
    r: D2D_RECT_F,
    look: Look,
    pal: &Palette,
    s: f32,
) -> D2D1_COLOR_F {
    let radius = px(4.0, s) as f32;
    // 地: 無し → fill-hover → fill-pressed、オンは on-fill → on-fill-hover → on-fill-pressed。その間を on で寄せる。
    let Look {
        hover, press, on, ..
    } = look;
    let off = mix(
        mix(fade(pal.fill_hover, 0.0), pal.fill_hover, hover),
        pal.fill_pressed,
        press,
    );
    let lit = mix(
        mix(pal.on_fill, pal.on_fill_hover, hover),
        pal.on_fill_pressed,
        press,
    );
    let fill = mix(off, lit, on);
    if !look.disabled && fill.a > 0.0 {
        fill_round(ctx, r, radius, fill);
    }
    if look.disabled {
        pal.text_disabled
    } else {
        mix(pal.text_primary, pal.on_content, on)
    }
}

/// フォーカス表示: 外形の外に隙間 1 (`focus-gap`) と輪 2 (`focus-ring`)、角丸は部品 + 3。
pub fn focus_ring(ctx: &ID2D1DeviceContext, r: D2D_RECT_F, radius: f32, pal: &Palette, s: f32) {
    let gap = stroke(1.0, s) as f32;
    let ring = stroke(2.0, s) as f32;
    let round = |inset: f32, rr: f32| D2D1_ROUNDED_RECT {
        rect: rectf(
            r.left - inset,
            r.top - inset,
            r.right + inset,
            r.bottom + inset,
        ),
        radiusX: rr,
        radiusY: rr,
    };
    unsafe {
        let gap_brush = ctx
            .CreateSolidColorBrush(&pal.focus_gap, None)
            .expect("brush");
        let ring_brush = ctx
            .CreateSolidColorBrush(&pal.focus_ring, None)
            .expect("brush");
        ctx.DrawRoundedRectangle(&round(gap / 2.0, radius + gap / 2.0), &gap_brush, gap, None);
        ctx.DrawRoundedRectangle(
            &round(gap + ring / 2.0, radius + px(3.0, s) as f32 - ring / 2.0),
            &ring_brush,
            ring,
            None,
        );
    }
}

// ---- ツールバー ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Item {
    Tool(&'static str),
    Sep,
    /// 32 × 32 の当たり。中身は呼び手が描く (注釈の色と線幅のボタン)。
    Slot,
    /// 56 × 32 の当たり。中身は呼び手が `label_button` で描く (拡大率のボタン)。
    Wide,
}

/// `Item::Wide` の幅 (100% の px)。「800%」が左右 8 の余白で入る。
pub const WIDE_W: f32 = 56.0;

/// 項目の左端と幅 (外形の左上から、物理 px)。最後の値は外形の幅。
/// 余白 4、ボタン 32、区切りの枠 13、項目どうしの gap 2 (Toolbar の表)。
pub fn toolbar_layout(items: &[Item], s: f32) -> (Vec<(i32, i32)>, i32) {
    let mut x = px(4.0, s);
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            x += px(2.0, s);
        }
        let w = match item {
            Item::Tool(_) | Item::Slot => px(32.0, s),
            Item::Wide => px(WIDE_W, s),
            Item::Sep => px(13.0, s),
        };
        out.push((x, w));
        x += w;
    }
    (out, x + px(4.0, s))
}

pub fn toolbar_height(s: f32) -> i32 {
    px(40.0, s)
}

/// ツールバーの中身 (地は `Acrylic::paint` が塗る)。`state(i)` が各項目の状態。
pub fn draw_toolbar<L: Into<Look>>(
    ctx: &ID2D1DeviceContext,
    items: &[Item],
    state: impl Fn(usize) -> L,
    pal: &Palette,
    s: f32,
) {
    let (layout, _) = toolbar_layout(items, s);
    let top = px(4.0, s) as f32;
    for (i, (item, (x, _))) in items.iter().zip(layout).enumerate() {
        match item {
            Item::Tool(icon) => tool_button(ctx, x as f32, top, icon, state(i), pal, s),
            Item::Slot | Item::Wide => {}
            Item::Sep => {
                // 線 1 × 18 を枠の左右 6 の間に、外形の上端から 11。
                let lw = stroke(1.0, s) as f32;
                let lx = (x + px(6.0, s)) as f32;
                let ly = px(11.0, s) as f32;
                fill_rect(
                    ctx,
                    rectf(lx, ly, lx + lw, ly + px(18.0, s) as f32),
                    pal.separator,
                );
            }
        }
    }
}

/// x (外形の左からの物理 px) にある項目の添字。
pub fn toolbar_hit(items: &[Item], s: f32, x: i32, y: i32) -> Option<usize> {
    let (layout, _) = toolbar_layout(items, s);
    let top = px(4.0, s);
    if y < top || y >= top + px(32.0, s) {
        return None;
    }
    layout
        .iter()
        .position(|&(l, w)| x >= l && x < l + w)
        .filter(|&i| items[i] != Item::Sep)
}

// ---- ツールチップ ----

/// 高さ 28、左右 10、名前とキーの gap 8。名前は `caption`、キーは `num` (`text-secondary`)。
pub struct Tooltip {
    name: Text,
    key: Option<Text>,
    pub w: i32,
    pub h: i32,
}

impl Tooltip {
    pub fn new(name: &str, key: &str, s: f32) -> Self {
        let name = super::text(name, Style::Caption, s);
        let key = (!key.is_empty()).then(|| super::text(key, Style::Num, s));
        let pad = px(10.0, s) as f32;
        let mut w = pad * 2.0 + name.w;
        if let Some(k) = &key {
            w += px(8.0, s) as f32 + k.w;
        }
        Self {
            name,
            key,
            w: w.ceil() as i32,
            h: px(28.0, s),
        }
    }

    pub fn draw(&self, ctx: &ID2D1DeviceContext, pal: &Palette, s: f32) {
        let pad = px(10.0, s) as f32;
        let y = ((self.h as f32 - self.name.h) / 2.0).round();
        draw_text(ctx, &self.name, pad, y, pal.text_primary);
        if let Some(k) = &self.key {
            draw_text(
                ctx,
                k,
                pad + self.name.w + px(8.0, s) as f32,
                y,
                pal.text_secondary,
            );
        }
    }
}

/// ツールチップの左上: 対象の下 8 に中央揃え。下に入らなければ上 8。作業領域の左右からはみ出さない。
pub fn place_tooltip(target: RECT, w: i32, h: i32, gap: i32, work: RECT) -> POINT {
    let cx = (target.left + target.right) / 2;
    let x = (cx - w / 2).clamp(work.left, (work.right - w).max(work.left));
    let below = target.bottom + gap;
    let y = if below + h <= work.bottom {
        below
    } else {
        target.top - gap - h
    };
    POINT { x, y }
}

// ---- スライダー (不透明度のポップアップ) ----

/// ポップアップ (248 × 72、余白 12) の寸法。
pub fn popup_size(s: f32) -> (i32, i32) {
    (px(248.0, s), px(72.0, s))
}

/// 溝の左端・幅・段の上端 (ポップアップの外形から)。1 行目 20 の 12 下に高さ 16 の段。
fn slider_geometry(s: f32) -> (f32, f32, f32) {
    let x0 = px(12.0, s) as f32;
    let w = px(224.0, s) as f32;
    let top = (px(12.0, s) + px(20.0, s) + px(12.0, s)) as f32;
    (x0, w, top)
}

/// 20〜100 を 10 刻み。つまみ (16) の中心は溝の両端から半径ぶん内側を動く。
fn knob_x(value: u32, s: f32) -> f32 {
    let (x0, w, _) = slider_geometry(s);
    let r = px(16.0, s) as f32 / 2.0;
    let t = (value.clamp(20, 100) - 20) as f32 / 80.0;
    x0 + r + t * (w - 2.0 * r)
}

/// ポップアップの x (外形から) を値にする。
pub fn slider_value(x: i32, s: f32) -> u32 {
    let (x0, w, _) = slider_geometry(s);
    let r = px(16.0, s) as f32 / 2.0;
    let t = ((x as f32 - x0 - r) / (w - 2.0 * r)).clamp(0.0, 1.0);
    20 + (t * 8.0).round() as u32 * 10
}

/// 不透明度のポップアップの中身。1 行目に題名 (`body`) と値 (`num-strong`) を両端に置く。
pub fn draw_slider_popup(ctx: &ID2D1DeviceContext, title: &str, value: u32, pal: &Palette, s: f32) {
    let (pw, _) = popup_size(s);
    let pad = px(12.0, s) as f32;
    let row = px(20.0, s) as f32;
    let t = super::text(title, Style::Body, s);
    draw_text(
        ctx,
        &t,
        pad,
        pad + ((row - t.h) / 2.0).round(),
        pal.text_primary,
    );
    let v = super::text(&format!("{value}%"), Style::NumStrong, s);
    draw_text(
        ctx,
        &v,
        pw as f32 - pad - v.w,
        pad + ((row - v.h) / 2.0).round(),
        pal.text_primary,
    );

    let (x0, w, top) = slider_geometry(s);
    let row_h = px(16.0, s) as f32;
    let th = px(4.0, s) as f32;
    let ty = top + ((row_h - th) / 2.0).round();
    let kx = knob_x(value, s);
    fill_round(ctx, rectf(x0, ty, x0 + w, ty + th), th / 2.0, pal.track);
    fill_round(ctx, rectf(x0, ty, kx, ty + th), th / 2.0, pal.on_fill);
    let kr = px(16.0, s) as f32 / 2.0;
    let cy = top + row_h / 2.0;
    let knob = |ctx: &ID2D1DeviceContext, c: D2D1_COLOR_F| unsafe {
        let brush = ctx.CreateSolidColorBrush(&c, None).expect("brush");
        ctx.FillEllipse(
            &D2D1_ELLIPSE {
                point: windows_numerics::Vector2 { X: kx, Y: cy },
                radiusX: kr,
                radiusY: kr,
            },
            &brush,
        );
    };
    // shadow-knob 0 1 4 rgba(0,0,0,.45): 標準偏差は blur 4 の半分。
    super::shadow(
        ctx,
        2.0 * s,
        super::rgba(0, 0, 0, 0.45),
        (0.0, s.round()),
        None,
        |c| knob(c, super::HL_DARK),
    );
    knob(ctx, pal.on_fill);
}

// ---- 注釈のポップアップ (Swatches) ----

/// 色のポップアップの幅 (スウォッチ 7 個)、線幅 (選択肢 5 つ)、文字の大きさ (選択肢 4 つ)。
pub const COLOR_POPUP_W: f32 = 268.0;
pub const WIDTH_POPUP_W: f32 = 200.0;
pub const SIZE_POPUP_W: f32 = 212.0;

/// 選択肢の行の上端: 上 14 + 見出しの行 16 + 行どうし 12。
fn choice_top(s: f32) -> i32 {
    px(14.0, s) + px(16.0, s) + px(12.0, s)
}

/// ポップアップの高さ: 選択肢の行 32 と下 16 まで。
pub fn choice_popup_h(s: f32) -> i32 {
    choice_top(s) + px(32.0, s) + px(16.0, s)
}

/// 32 × 32 の選択肢を gap 2 で並べた i 番目の左上 (外形から)。
fn choice_at(i: usize, s: f32) -> (i32, i32) {
    (
        px(16.0, s) + i as i32 * (px(32.0, s) + px(2.0, s)),
        choice_top(s),
    )
}

/// 32 × 32 の選択肢 n 個のどれに当たるか。
pub fn choice_hit(n: usize, x: i32, y: i32, s: f32) -> Option<usize> {
    let b = px(32.0, s);
    (0..n).find(|&i| {
        let (l, t) = choice_at(i, s);
        x >= l && x < l + b && y >= t && y < t + b
    })
}

/// 見出し (左) と値 (右)。どちらも `caption`、`text-secondary`。
pub fn popup_head(
    ctx: &ID2D1DeviceContext,
    title: &str,
    value: &str,
    w: f32,
    pal: &Palette,
    s: f32,
) {
    let y = px(14.0, s) as f32;
    let pad = px(16.0, s) as f32;
    let t = super::text(title, Style::Caption, s);
    draw_text(ctx, &t, pad, y, pal.text_secondary);
    let v = super::text(value, Style::Caption, s);
    draw_text(ctx, &v, w - pad - v.w, y, pal.text_secondary);
}

fn stroke_round(ctx: &ID2D1DeviceContext, r: D2D_RECT_F, radius: f32, w: f32, c: D2D1_COLOR_F) {
    unsafe {
        let brush = ctx.CreateSolidColorBrush(&c, None).expect("brush");
        ctx.DrawRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: r,
                radiusX: radius,
                radiusY: radius,
            },
            &brush,
            w,
            None,
        );
    }
}

/// 線の外形が `r` に収まる角丸の線 (太さ `w`、外形の角丸 `radius`)。
fn stroke_inside(ctx: &ID2D1DeviceContext, r: D2D_RECT_F, radius: f32, w: f32, c: D2D1_COLOR_F) {
    let h = w / 2.0;
    stroke_round(
        ctx,
        rectf(r.left + h, r.top + h, r.right - h, r.bottom - h),
        radius - h,
        w,
        c,
    );
}

/// スウォッチ: 当たり 32 × 32 (角丸 4) の中央に四角 16 (角丸 4、内側 1px の `border-swatch`)。
/// 選んだ色は外形 22・太さ 2・角丸 8 の `on-fill` の輪。(x, y) は当たりの左上。
#[allow(clippy::too_many_arguments)]
pub fn swatch(
    ctx: &ID2D1DeviceContext,
    x: f32,
    y: f32,
    color: D2D1_COLOR_F,
    state: BtnState,
    selected: bool,
    pal: &Palette,
    s: f32,
) {
    let b = px(32.0, s) as f32;
    let back = match state {
        BtnState::Hover => Some(pal.fill_hover),
        BtnState::Pressed | BtnState::Open => Some(pal.fill_pressed),
        _ => None,
    };
    if let Some(c) = back {
        fill_round(ctx, rectf(x, y, x + b, y + b), px(4.0, s) as f32, c);
    }
    let q = px(16.0, s) as f32;
    let o = ((b - q) / 2.0).round();
    let r4 = px(4.0, s) as f32;
    let sq = rectf(x + o, y + o, x + o + q, y + o + q);
    fill_round(ctx, sq, r4, color);
    stroke_inside(ctx, sq, r4, stroke(1.0, s) as f32, pal.border_swatch);
    if selected {
        let ring = px(22.0, s) as f32;
        let ro = ((b - ring) / 2.0).round();
        stroke_inside(
            ctx,
            rectf(x + ro, y + ro, x + ro + ring, y + ro + ring),
            px(8.0, s) as f32,
            stroke(2.0, s) as f32,
            pal.on_fill,
        );
    }
}

/// 線幅の見本: 長さ 16、太さ `width` (100% の px)、角丸 2。(cx, cy) が中心。
pub fn width_sample(
    ctx: &ID2D1DeviceContext,
    cx: f32,
    cy: f32,
    width: f32,
    color: D2D1_COLOR_F,
    s: f32,
) {
    let l = px(16.0, s) as f32;
    let t = px(width, s) as f32;
    let (x, y) = ((cx - l / 2.0).round(), (cy - t / 2.0).round());
    fill_round(
        ctx,
        rectf(x, y, x + l, y + t),
        (px(2.0, s) as f32).min(t / 2.0),
        color,
    );
}

/// スウォッチの行。`sel` は選んだ色、`hover` はホバー中の添字。
pub fn draw_color_choices(
    ctx: &ID2D1DeviceContext,
    colors: &[D2D1_COLOR_F],
    sel: usize,
    hover: Option<usize>,
    pal: &Palette,
    s: f32,
) {
    for (i, c) in colors.iter().enumerate() {
        let (x, y) = choice_at(i, s);
        let st = if hover == Some(i) {
            BtnState::Hover
        } else {
            BtnState::Normal
        };
        swatch(ctx, x as f32, y as f32, *c, st, i == sel, pal, s);
    }
}

/// 線幅の選択肢 (32 × 32 のボタンに見本の線)。選んだ値はオンの面。
pub fn draw_width_choices(
    ctx: &ID2D1DeviceContext,
    widths: &[u32],
    sel: usize,
    hover: Option<usize>,
    pal: &Palette,
    s: f32,
) {
    let b = px(32.0, s) as f32;
    for (i, w) in widths.iter().enumerate() {
        let (x, y) = choice_at(i, s);
        let (x, y) = (x as f32, y as f32);
        let (fill, ink) = if i == sel {
            (Some(pal.on_fill), pal.on_content)
        } else if hover == Some(i) {
            (Some(pal.fill_hover), pal.text_primary)
        } else {
            (None, pal.text_primary)
        };
        if let Some(c) = fill {
            fill_round(ctx, rectf(x, y, x + b, y + b), px(4.0, s) as f32, c);
        }
        width_sample(ctx, x + b / 2.0, y + b / 2.0, *w as f32, ink, s);
    }
}

/// 選択肢の並びの項目 i の矩形 (外形から): 並びは左右 16 の内側、内側の余白 2、項目の高さ 28、gap 2、等幅。
fn seg_item(i: usize, n: usize, popup_w: f32, s: f32) -> D2D_RECT_F {
    let pad = px(2.0, s) as f32;
    let gap = px(2.0, s) as f32;
    let left = px(16.0, s) as f32 + pad;
    let inner = px(popup_w, s) as f32 - 2.0 * px(16.0, s) as f32 - 2.0 * pad;
    let w = (inner - gap * (n as f32 - 1.0)) / n as f32;
    let x = left + i as f32 * (w + gap);
    let y = choice_top(s) as f32 + pad;
    rectf(x, y, x + w, y + px(28.0, s) as f32)
}

pub fn seg_hit(n: usize, popup_w: f32, x: i32, y: i32, s: f32) -> Option<usize> {
    let (x, y) = (x as f32, y as f32);
    (0..n).find(|&i| {
        let r = seg_item(i, n, popup_w, s);
        x >= r.left && x < r.right && y >= r.top && y < r.bottom
    })
}

/// 選択肢の並び (文字の大きさ): 地 `fill-hover` 角丸 8、項目は角丸 4、値は `num`。選んだ値はオンの面。
#[allow(clippy::too_many_arguments)]
pub fn draw_seg(
    ctx: &ID2D1DeviceContext,
    labels: &[String],
    sel: usize,
    hover: Option<usize>,
    popup_w: f32,
    pal: &Palette,
    s: f32,
) {
    let n = labels.len();
    let x0 = px(16.0, s) as f32;
    let y0 = choice_top(s) as f32;
    fill_round(
        ctx,
        rectf(x0, y0, px(popup_w, s) as f32 - x0, y0 + px(32.0, s) as f32),
        px(8.0, s) as f32,
        pal.fill_hover,
    );
    for (i, label) in labels.iter().enumerate() {
        let r = seg_item(i, n, popup_w, s);
        let (fill, ink) = if i == sel {
            (Some(pal.on_fill), pal.on_content)
        } else if hover == Some(i) {
            (Some(pal.fill_hover), pal.text_primary)
        } else {
            (None, pal.text_primary)
        };
        if let Some(c) = fill {
            fill_round(ctx, r, px(4.0, s) as f32, c);
        }
        let t = super::text(label, Style::Num, s);
        draw_text(
            ctx,
            &t,
            ((r.left + r.right - t.w) / 2.0).round(),
            (r.top + (r.bottom - r.top - t.h) / 2.0).round(),
            ink,
        );
    }
}

// ---- D2D 半透明のラベル ----

pub enum Part {
    Text(Text, D2D1_COLOR_F),
    /// 16 のアイコン。
    Icon(&'static str, D2D1_COLOR_F),
    Gap(f32),
}

/// 高さ 24、左右 11、角丸 4、`surface-veil`。部品どうしの間は呼び手が `Gap` で入れる。
pub struct Label {
    parts: Vec<Part>,
    pub w: i32,
    pub h: i32,
}

impl Label {
    pub fn new(parts: Vec<Part>, s: f32) -> Self {
        let inner: f32 = parts
            .iter()
            .map(|p| match p {
                Part::Text(t, _) => t.w,
                Part::Icon(..) => 16.0 * s,
                Part::Gap(g) => *g,
            })
            .sum();
        Self {
            parts,
            w: (inner + 2.0 * px(11.0, s) as f32).ceil() as i32,
            h: px(24.0, s),
        }
    }

    pub fn draw(&self, ctx: &ID2D1DeviceContext, x: f32, y: f32, pal: &Palette, s: f32) {
        fill_round(
            ctx,
            rectf(x, y, x + self.w as f32, y + self.h as f32),
            px(4.0, s) as f32,
            pal.surface_veil,
        );
        let mut cx = x + px(11.0, s) as f32;
        for p in &self.parts {
            match p {
                Part::Text(t, c) => {
                    draw_text(ctx, t, cx, y + ((self.h as f32 - t.h) / 2.0).round(), *c);
                    cx += t.w;
                }
                Part::Icon(id, c) => {
                    let size = 16.0 * s;
                    draw_icon(ctx, id, cx, y + (self.h as f32 - size) / 2.0, size, *c);
                    cx += size;
                }
                Part::Gap(g) => cx += g,
            }
        }
    }
}

// ---- ドロップダウンと一覧 (Controls) ----

/// ドロップダウン (高さ 32、角丸 4、左 12・右 8、値と chevron の gap 8)。`hot` はホバーか一覧を開いている間。
/// 地は `control-fill`、外形の内側 1px の `control-stroke`、下辺の内側 1px の `hairline`。
/// 値は `body` の tabular figures (`BodyTabular`)。
pub fn dropdown(
    ctx: &ID2D1DeviceContext,
    r: D2D_RECT_F,
    value: &str,
    hot: f32,
    pal: &Palette,
    s: f32,
) {
    let radius = px(4.0, s) as f32;
    fill_round(
        ctx,
        r,
        radius,
        mix(pal.control_fill, pal.control_fill_hover, hot),
    );
    let line = stroke(1.0, s) as f32;
    stroke_inside(ctx, r, radius, line, pal.control_stroke);
    fill_rect(
        ctx,
        rectf(r.left + radius, r.bottom - line, r.right - radius, r.bottom),
        pal.hairline,
    );
    let h = r.bottom - r.top;
    let t = super::text(value, Style::BodyTabular, s);
    draw_text(
        ctx,
        &t,
        r.left + px(12.0, s) as f32,
        r.top + ((h - t.h) / 2.0).round(),
        pal.text_primary,
    );
    let icon = px(16.0, s) as f32;
    draw_icon(
        ctx,
        "chevron-down",
        r.right - px(8.0, s) as f32 - icon,
        r.top + ((h - icon) / 2.0).round(),
        icon,
        pal.text_primary,
    );
}

/// 一覧の大きさ: 内側の余白 4、行 32、行どうし 2。幅は `min_w` (100% の px、最小 180) と部品の幅の大きい方。
pub fn list_size(n: usize, width: i32, s: f32) -> (i32, i32) {
    let w = width.max(px(180.0, s));
    let h = 2 * px(4.0, s) + n as i32 * px(32.0, s) + (n as i32 - 1).max(0) * px(2.0, s);
    (w, h)
}

fn list_row(i: usize, w: i32, s: f32) -> RECT {
    let top = px(4.0, s) + i as i32 * (px(32.0, s) + px(2.0, s));
    RECT {
        left: px(4.0, s),
        top,
        right: w - px(4.0, s),
        bottom: top + px(32.0, s),
    }
}

pub fn list_hit(n: usize, w: i32, x: i32, y: i32, s: f32) -> Option<usize> {
    (0..n).find(|&i| {
        let r = list_row(i, w, s);
        x >= r.left && x < r.right && y >= r.top && y < r.bottom
    })
}

/// 一覧の中身 (地は `Acrylic::paint`)。行は `body` の tabular figures (`BodyTabular`)。
/// `sel` は印を付ける行 (今の値が一覧に無ければ None)。
/// 選んだ行は地を塗らず、左の印 3 × 16 (`on-fill`) と `body-strong` の tabular figures (`BodyStrongTabular`)。
/// ホバーの行は `fill-subtle`。
pub fn draw_list(
    ctx: &ID2D1DeviceContext,
    labels: &[String],
    sel: Option<usize>,
    hover: Option<usize>,
    w: i32,
    pal: &Palette,
    s: f32,
) {
    for (i, label) in labels.iter().enumerate() {
        let r = list_row(i, w, s);
        let rf = rectf(r.left as f32, r.top as f32, r.right as f32, r.bottom as f32);
        let selected = sel == Some(i);
        if hover == Some(i) && !selected {
            fill_round(ctx, rf, px(4.0, s) as f32, pal.fill_subtle);
        }
        if selected {
            let top = rf.top + px(8.0, s) as f32;
            fill_round(
                ctx,
                rectf(
                    rf.left,
                    top,
                    rf.left + px(3.0, s) as f32,
                    top + px(16.0, s) as f32,
                ),
                px(1.5, s) as f32,
                pal.on_fill,
            );
        }
        let style = if selected {
            Style::BodyStrongTabular
        } else {
            Style::BodyTabular
        };
        let t = super::text(label, style, s);
        draw_text(
            ctx,
            &t,
            rf.left + px(12.0, s) as f32,
            rf.top + ((rf.bottom - rf.top - t.h) / 2.0).round(),
            pal.text_primary,
        );
    }
}

// ---- 設定窓の部品 (Controls・Buttons の設定の文字のボタン) ----

/// `a` から `b` へ `t` (0..=1) だけ寄せた色。遷移の途中の色に使う。
pub fn mix(a: D2D1_COLOR_F, b: D2D1_COLOR_F, t: f32) -> D2D1_COLOR_F {
    let t = t.clamp(0.0, 1.0);
    D2D1_COLOR_F {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// 不透明度を `t` 倍した色。
pub fn fade(c: D2D1_COLOR_F, t: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        a: c.a * t.clamp(0.0, 1.0),
        ..c
    }
}

/// 部品の地: `control-fill` (ホバー `hot` = 1 で `control-fill-hover`)、外形の内側 1px の `control-stroke`、下辺の内側 1px の `hairline`。
fn control_face(ctx: &ID2D1DeviceContext, r: D2D_RECT_F, hot: f32, pal: &Palette, s: f32) {
    let radius = px(4.0, s) as f32;
    fill_round(
        ctx,
        r,
        radius,
        mix(pal.control_fill, pal.control_fill_hover, hot),
    );
    let line = stroke(1.0, s) as f32;
    stroke_inside(ctx, r, radius, line, pal.control_stroke);
    fill_rect(
        ctx,
        rectf(r.left + radius, r.bottom - line, r.right - radius, r.bottom),
        pal.hairline,
    );
}

/// トグルの外形 (40 × 20) の大きさ。
pub fn toggle_size(s: f32) -> (i32, i32) {
    (px(40.0, s), px(20.0, s))
}

/// トグル。(x, y) は外形の左上。オフは `border-strong` の枠と `text-secondary` のつまみ 12、
/// ホバーは `fill-hover` の地と `text-primary` のつまみ 14。オンは `on-fill` の面と `on-content` のつまみ。
/// `on` はオフ 0 からオン 1 への途中 (つまみの位置は 1 を少し越えてよい)、`hover` はホバーの度合い (0..=1)。
pub fn toggle(
    ctx: &ID2D1DeviceContext,
    x: f32,
    y: f32,
    on: f32,
    hover: f32,
    pal: &Palette,
    s: f32,
) {
    let (w, h) = toggle_size(s);
    let r = rectf(x, y, x + w as f32, y + h as f32);
    let radius = h as f32 / 2.0;
    let p = on.clamp(0.0, 1.0);
    let small = px(12.0, s) as f32;
    let thumb = small + (px(14.0, s) as f32 - small) * hover;
    let inset = px(4.0, s) as f32;
    // つまみの中心は、つまみ 12 のときの位置から位置へ滑らせ、太らせても動かさない。
    let cy = y + h as f32 / 2.0;
    let off_x = x + inset + small / 2.0;
    let on_x = x + w as f32 - inset - small / 2.0;
    let cx = off_x + (on_x - off_x) * on;
    if p < 1.0 {
        fill_round(ctx, r, radius, fade(pal.fill_hover, hover * (1.0 - p)));
        stroke_inside(
            ctx,
            r,
            radius,
            stroke(1.0, s) as f32,
            fade(pal.border_strong, 1.0 - p),
        );
    }
    if p > 0.0 {
        let face = mix(pal.on_fill, pal.on_fill_hover, hover);
        fill_round(ctx, r, radius, fade(face, p));
    }
    let knob = mix(
        mix(pal.text_secondary, pal.text_primary, hover),
        pal.on_content,
        p,
    );
    unsafe {
        let brush = ctx.CreateSolidColorBrush(&knob, None).expect("brush");
        ctx.FillEllipse(
            &D2D1_ELLIPSE {
                point: windows_numerics::Vector2 { X: cx, Y: cy },
                radiusX: thumb / 2.0,
                radiusY: thumb / 2.0,
            },
            &brush,
        );
    }
}

/// 設定の文字のボタンの幅 (左右 12 + `body`、最小 72)。
pub fn text_button_width(label: &str, s: f32) -> i32 {
    (super::text(label, Style::Body, s).w.ceil() as i32 + 2 * px(12.0, s)).max(px(72.0, s))
}

/// 設定の文字のボタン (変更…)。高さ 32、部品の地。
pub fn text_button(
    ctx: &ID2D1DeviceContext,
    r: D2D_RECT_F,
    label: &str,
    hot: f32,
    pal: &Palette,
    s: f32,
) {
    control_face(ctx, r, hot, pal, s);
    let t = super::text(label, Style::Body, s);
    draw_text(
        ctx,
        &t,
        ((r.left + r.right - t.w) / 2.0).round(),
        r.top + ((r.bottom - r.top - t.h) / 2.0).round(),
        pal.text_primary,
    );
}

/// ホットキー記録欄の状態。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotkeyField {
    /// 文字列 1 つを右に寄せる。空は未割当て (`text-secondary`)。
    Idle(String),
    /// 押している修飾キーのキーキャップと案内。
    Recording(Vec<String>),
    /// 元に戻したキーのキーキャップ。
    Failed(Vec<String>),
}

fn keycaps_width(caps: &[String], s: f32) -> f32 {
    caps.iter()
        .map(|c| super::text(c, Style::CaptionStrong, s).w + 2.0 * px(8.0, s) as f32)
        .sum::<f32>()
        + px(4.0, s) as f32 * caps.len().saturating_sub(1) as f32
}

/// ホットキー記録欄の幅 (最小 200)。`unassigned`・`press` は未割当てと記録中の案内。
/// 記録中は、修飾キーを押すまで案内、押したらキーキャップだけを出す。
pub fn hotkey_field_width(f: &HotkeyField, unassigned: &str, press: &str, s: f32) -> i32 {
    let w = match f {
        HotkeyField::Idle(t) => {
            let t = if t.is_empty() { unassigned } else { t };
            super::text(t, Style::BodyTabular, s).w + 2.0 * px(12.0, s) as f32
        }
        HotkeyField::Recording(caps) if caps.is_empty() => {
            super::text(press, Style::Body, s).w + 2.0 * px(12.0, s) as f32
        }
        HotkeyField::Recording(caps) => 2.0 * px(6.0, s) as f32 + keycaps_width(caps, s),
        HotkeyField::Failed(caps) => 2.0 * px(6.0, s) as f32 + keycaps_width(caps, s),
    };
    (w.ceil() as i32).max(px(200.0, s))
}

/// ホットキー記録欄。`r` は外形 (高さ 32)。
pub fn hotkey_field(
    ctx: &ID2D1DeviceContext,
    r: D2D_RECT_F,
    f: &HotkeyField,
    unassigned: &str,
    press: &str,
    pal: &Palette,
    s: f32,
) {
    let radius = px(4.0, s) as f32;
    let h = r.bottom - r.top;
    let mid = |th: f32| r.top + ((h - th) / 2.0).round();
    let caps = match f {
        HotkeyField::Idle(text) => {
            let (t, c) = if text.is_empty() {
                (unassigned, pal.text_secondary)
            } else {
                (text.as_str(), pal.text_primary)
            };
            let t = super::text(t, Style::BodyTabular, s);
            draw_text(ctx, &t, r.right - px(12.0, s) as f32 - t.w, mid(t.h), c);
            return;
        }
        HotkeyField::Recording(caps) => {
            fill_round(ctx, r, radius, pal.control_fill_active);
            stroke_inside(ctx, r, radius, stroke(2.0, s) as f32, pal.focus_ring);
            caps
        }
        HotkeyField::Failed(caps) => {
            fill_round(ctx, r, radius, pal.control_fill);
            stroke_inside(ctx, r, radius, stroke(2.0, s) as f32, pal.danger);
            caps
        }
    };
    let mut x = r.left + px(6.0, s) as f32;
    let kh = px(22.0, s) as f32;
    for c in caps {
        let t = super::text(c, Style::CaptionStrong, s);
        let w = t.w + 2.0 * px(8.0, s) as f32;
        let ky = mid(kh);
        fill_round(ctx, rectf(x, ky, x + w, ky + kh), radius, pal.keycap);
        draw_text(
            ctx,
            &t,
            x + px(8.0, s) as f32,
            ky + ((kh - t.h) / 2.0).round(),
            pal.text_primary,
        );
        x += w + px(4.0, s) as f32;
    }
    if matches!(f, HotkeyField::Recording(_)) && caps.is_empty() {
        let t = super::text(press, Style::Body, s);
        draw_text(ctx, &t, x + px(6.0, s) as f32, mid(t.h), pal.text_secondary);
    }
}

/// ヒントかエラー文の段: エラーは記号 16 (`danger`) と文字 (`text-primary`)、ヒントは文字だけ (`text-secondary`)。
/// 文字は幅 `max_w` で折り返す (記号の分も含む)。
#[allow(clippy::too_many_arguments)]
pub fn hint(
    ctx: &ID2D1DeviceContext,
    x: f32,
    y: f32,
    max_w: f32,
    text: &str,
    error: bool,
    pal: &Palette,
    s: f32,
) {
    let t = super::text_wrapped(text, Style::Caption, s, hint_text_w(max_w, error, s));
    let mut tx = x;
    if error {
        let icon = px(16.0, s) as f32;
        draw_icon(ctx, "error", x, y, icon, pal.danger);
        tx += icon + px(6.0, s) as f32;
    }
    let c = if error {
        pal.text_primary
    } else {
        pal.text_secondary
    };
    draw_text(ctx, &t, tx, y, c);
}

/// ヒントの文字の折り返し幅 (エラーは記号 16 と gap 6 を引く)。
pub fn hint_text_w(max_w: f32, error: bool, s: f32) -> f32 {
    if error {
        max_w - px(22.0, s) as f32
    } else {
        max_w
    }
}

/// ナビの行 (一覧の行と同じ: 高さ 32、左右 12、アイコン 18 と名前の gap 12)。
/// 選んだ行は地を塗らず `body-strong` (左の印は `nav_marker` が別に描いて滑らせる)。ホバーは `fill-subtle`。
#[allow(clippy::too_many_arguments)]
pub fn nav_row(
    ctx: &ID2D1DeviceContext,
    r: D2D_RECT_F,
    icon: &'static str,
    label: &str,
    selected: bool,
    hover: f32,
    pal: &Palette,
    s: f32,
) {
    if hover > 0.0 {
        fill_round(ctx, r, px(4.0, s) as f32, fade(pal.fill_subtle, hover));
    }
    let h = r.bottom - r.top;
    let size = px(18.0, s) as f32;
    let x = r.left + px(12.0, s) as f32;
    let ink = mix(
        pal.text_secondary,
        pal.text_primary,
        if selected { 1.0 } else { hover },
    );
    draw_icon(ctx, icon, x, r.top + ((h - size) / 2.0).round(), size, ink);
    let style = if selected {
        Style::BodyStrong
    } else {
        Style::Body
    };
    let t = super::text(label, style, s);
    draw_text(
        ctx,
        &t,
        x + size + px(12.0, s) as f32,
        r.top + ((h - t.h) / 2.0).round(),
        pal.text_primary,
    );
}

/// ナビの選択の印 3 × 16 (`on-fill`)。(x, row_top) は印を付ける行の左上。
pub fn nav_marker(ctx: &ID2D1DeviceContext, x: f32, row_top: f32, pal: &Palette, s: f32) {
    let top = row_top + px(8.0, s) as f32;
    fill_round(
        ctx,
        rectf(x, top, x + px(3.0, s) as f32, top + px(16.0, s) as f32),
        px(1.5, s) as f32,
        pal.on_fill,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIN_TOOLBAR: [Item; 10] = [
        Item::Tool("copy"),
        Item::Tool("save"),
        Item::Tool("annotate"),
        Item::Sep,
        Item::Tool("opacity"),
        Item::Sep,
        Item::Tool("clickthrough"),
        Item::Tool("topmost"),
        Item::Sep,
        Item::Tool("close"),
    ];

    #[test]
    fn pin_toolbar_dimensions() {
        // 拡大率のボタンが無い並び (`PIN_TOOLBAR`): 外形の幅 289、不透明度のボタンの左端は 121、区切りを挟んだボタンは 17 離れる。
        // DS の Toolbar の表は拡大率のボタンを含む 347 (t261107-d93c)。
        let (layout, w) = toolbar_layout(&PIN_TOOLBAR, 1.0);
        assert_eq!(w, 289);
        assert_eq!(layout[4], (121, 32));
        assert_eq!(layout[4].0 - (layout[2].0 + layout[2].1), 17);
        assert_eq!(toolbar_height(1.5), 60);
        let (_, w15) = toolbar_layout(&PIN_TOOLBAR, 1.5);
        // 6 + 48×7 + 20×3 + 3×9 + 6
        assert_eq!(w15, 6 + 48 * 7 + 20 * 3 + 3 * 9 + 6);
    }

    #[test]
    fn annotation_toolbar_dimensions() {
        // AnnotationToolbar の表: 色のボタンは左から 355、線幅のボタンは 389、完了 (ツールのボタン) は 521。外形 557。
        let t = Item::Tool("x");
        let items = [
            t,
            t,
            t,
            t,
            Item::Sep,
            t,
            t,
            Item::Sep,
            t,
            t,
            t,
            Item::Sep,
            Item::Slot,
            Item::Slot,
            Item::Sep,
            t,
            t,
            Item::Sep,
            t,
        ];
        let (layout, w) = toolbar_layout(&items, 1.0);
        assert_eq!(layout[12], (355, 32));
        assert_eq!(layout[13], (389, 32));
        assert_eq!(layout[18], (521, 32));
        assert_eq!(w, 557);
        // 色・線幅・完了も押せる。
        assert_eq!(toolbar_hit(&items, 1.0, 360, 20), Some(12));
        assert_eq!(toolbar_hit(&items, 1.0, 540, 20), Some(18));
    }

    #[test]
    fn popup_choices() {
        // 色 268 = 16 + 7 × 32 + 6 × 2 + 16、線幅 200 = 16 + 5 × 32 + 4 × 2 + 16。高さ 14 + 16 + 12 + 32 + 16。
        assert_eq!(choice_at(6, 1.0).0 + 32 + 16, COLOR_POPUP_W as i32);
        assert_eq!(choice_at(4, 1.0).0 + 32 + 16, WIDTH_POPUP_W as i32);
        assert_eq!(choice_popup_h(1.0), 90);
        assert_eq!(choice_hit(7, 16, 42, 1.0), Some(0));
        assert_eq!(choice_hit(7, 48, 50, 1.0), None); // gap
        assert_eq!(choice_hit(7, 50, 50, 1.0), Some(1));
        // 文字の大きさ: 4 つの項目が並びの内側 (左右 18) に等幅。
        let last = seg_item(3, 4, SIZE_POPUP_W, 1.0);
        assert!((last.right - (SIZE_POPUP_W - 18.0)).abs() < 1e-3);
        assert_eq!(seg_hit(4, SIZE_POPUP_W, 20, 50, 1.0), Some(0));
        assert_eq!(seg_hit(4, SIZE_POPUP_W, 190, 50, 1.0), Some(3));
    }

    #[test]
    fn toolbar_hit_skips_separators() {
        assert_eq!(toolbar_hit(&PIN_TOOLBAR, 1.0, 121, 20), Some(4));
        assert_eq!(toolbar_hit(&PIN_TOOLBAR, 1.0, 112, 20), None); // 区切り
        assert_eq!(toolbar_hit(&PIN_TOOLBAR, 1.0, 121, 2), None); // 上の余白
    }

    #[test]
    fn slider_steps() {
        for v in (20..=100).step_by(10) {
            assert_eq!(slider_value(knob_x(v, 1.0).round() as i32, 1.0), v);
        }
        assert_eq!(slider_value(-50, 1.0), 20);
        assert_eq!(slider_value(1000, 1.0), 100);
    }

    #[test]
    fn tooltip_placement() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let target = RECT {
            left: 100,
            top: 100,
            right: 132,
            bottom: 132,
        };
        // 下 8 に中央揃え。
        assert_eq!(
            place_tooltip(target, 80, 28, 8, work),
            POINT { x: 76, y: 140 }
        );
        // 下に入らない → 上 8。
        let low = RECT {
            left: 100,
            top: 1000,
            right: 132,
            bottom: 1032,
        };
        assert_eq!(place_tooltip(low, 80, 28, 8, work).y, 1000 - 8 - 28);
        // 左端からはみ出さない。
        let edge = RECT {
            left: 0,
            top: 100,
            right: 32,
            bottom: 132,
        };
        assert_eq!(place_tooltip(edge, 80, 28, 8, work).x, 0);
    }

    #[test]
    fn svg_path_subset() {
        let segs = parse_path("M9 2.875A4 4 0 0 1 13 6.75V11.25H-1.5Z M1 2L3 4");
        assert_eq!(
            segs,
            vec![
                Seg::Move(9.0, 2.875),
                Seg::Arc(4.0, 4.0, 0.0, false, true, 13.0, 6.75),
                Seg::Line(13.0, 11.25),
                Seg::Line(-1.5, 11.25),
                Seg::Close,
                Seg::Move(1.0, 2.0),
                Seg::Line(3.0, 4.0),
            ]
        );
        // DS の icons.json から読める。
        let (st, fi) = icon_paths("opacity");
        assert_eq!((st.len(), fi.len()), (1, 1));
    }
}
