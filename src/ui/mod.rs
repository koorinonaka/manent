//! 描画の土台: D3D11 / D2D / DirectComposition / DirectWrite / WIC の初期化、テーマ、
//! DPI の丸め、書体、トークンの色。部品は `widgets`。
//! UI はメインスレッドだけで動く (`gfx()` はスレッドローカル)。

pub mod anim;
pub mod widgets;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    MonitorFromRect,
};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetCursorPos, GetWindowRect, GetWindowThreadProcessId, HWND_NOTOPMOST,
    HWND_TOPMOST, IsWindow, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowDisplayAffinity,
    SetWindowPos, WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WindowFromPoint,
};
use windows::core::{Interface, PCWSTR, Result, w};

pub struct Gfx {
    pub d3d: ID3D11Device,
    pub d3d_ctx: ID3D11DeviceContext,
    pub dxgi: IDXGIDevice,
    pub dxgi_factory: IDXGIFactory2,
    pub factory: ID2D1Factory1,
    pub d2d: ID2D1Device,
    pub dcomp: IDCompositionDevice,
    pub dwrite: IDWriteFactory2,
    pub wic: IWICImagingFactory,
    fallback: IDWriteFontFallback,
}

thread_local! {
    static GFX: OnceCell<&'static Gfx> = const { OnceCell::new() };
    /// `text_in` の書式: (Style, s のビット, 折り返し) ごとに 1 個。
    static FORMATS: RefCell<HashMap<(Style, u32, bool), IDWriteTextFormat>> = RefCell::new(HashMap::new());
}

/// メインスレッドの描画デバイス。初回に作る (COM は呼び手が初期化済み)。
/// ponytail: デバイスロスト (GPU のリセット) からの復帰はしない。起き得たら作り直しを足す。
pub fn gfx() -> &'static Gfx {
    GFX.with(|g| *g.get_or_init(|| Box::leak(Box::new(Gfx::new().expect("graphics init failed")))))
}

impl Gfx {
    fn new() -> Result<Self> {
        crate::insight_scope!("gfx_init");
        unsafe {
            let mut d3d = None;
            let mut d3d_ctx = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_0]),
                D3D11_SDK_VERSION,
                Some(&mut d3d),
                None,
                Some(&mut d3d_ctx),
            )?;
            let d3d: ID3D11Device = d3d.unwrap();
            let dxgi: IDXGIDevice = d3d.cast()?;
            let dxgi_factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d = factory.CreateDevice(&dxgi)?;
            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
            let dwrite: IDWriteFactory2 = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            // 和文は Yu Gothic UI に割り当て、残りはシステムの代替に任せる。
            let builder = dwrite.CreateFontFallbackBuilder()?;
            let cjk = [
                DWRITE_UNICODE_RANGE {
                    first: 0x3000,
                    last: 0x30FF,
                },
                DWRITE_UNICODE_RANGE {
                    first: 0x3400,
                    last: 0x4DBF,
                },
                DWRITE_UNICODE_RANGE {
                    first: 0x4E00,
                    last: 0x9FFF,
                },
                DWRITE_UNICODE_RANGE {
                    first: 0xFF00,
                    last: 0xFFEF,
                },
            ];
            builder.AddMapping(
                &cjk,
                &[w!("Yu Gothic UI").as_ptr()],
                None,
                w!("ja-jp"),
                None,
                1.0,
            )?;
            builder.AddMappings(&dwrite.GetSystemFontFallback()?)?;
            let fallback = builder.CreateFontFallback()?;
            Ok(Self {
                d3d,
                d3d_ctx: d3d_ctx.unwrap(),
                dxgi,
                dxgi_factory,
                factory,
                d2d,
                dcomp,
                dwrite,
                wic,
                fallback,
            })
        }
    }
}

// ---- DPI ----

/// 窓のいまのモニタの倍率 s = DPI / 96。
pub fn scale_of(hwnd: HWND) -> f32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
}

/// 長さ・余白・角丸: round(値×s)、0.5 は切り上げ。
pub fn px(v: f32, s: f32) -> i32 {
    // 1.25×2 のような境目を浮動小数の誤差で落とさない。
    (v * s + 0.5 + 1e-4).floor() as i32
}

/// `stroke-1`・`stroke-2`: max(1, round(値×s))。
pub fn stroke(v: f32, s: f32) -> i32 {
    px(v, s).max(1)
}

// ---- テーマ ----

fn reg_dword(name: PCWSTR) -> Option<u32> {
    let mut value = 0u32;
    let mut size = 4u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            name,
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    }
    .ok()
    .ok()
    .map(|()| value)
}

/// アプリのテーマ (`AppsUseLightTheme`)。値が無ければライト。
pub fn apps_dark() -> bool {
    // debug ビルドは `ODILE_THEME=light|dark` で上書きする (OS の設定を変えずに両方の配色を撮るため)。
    if cfg!(debug_assertions)
        && let Ok(t) = std::env::var("ODILE_THEME")
    {
        return t != "light";
    }
    reg_dword(w!("AppsUseLightTheme")) == Some(0)
}

/// タスクバーの明暗 (`SystemUsesLightTheme`)。値が無ければダーク (Win11 の既定)。
pub fn taskbar_dark() -> bool {
    reg_dword(w!("SystemUsesLightTheme")) != Some(1)
}

// ---- 色 (tokens.json) ----

pub const fn rgba(r: u8, g: u8, b: u8, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

pub const fn hex(v: u32) -> D2D1_COLOR_F {
    rgba((v >> 16) as u8, (v >> 8) as u8, v as u8, 1.0)
}

pub const SCRIM: D2D1_COLOR_F = rgba(0, 0, 0, 0.38);
pub const HL_CORE: D2D1_COLOR_F = hex(0xFFFFFF);
pub const HL_DARK: D2D1_COLOR_F = hex(0x000000);
pub const HL_RIM: D2D1_COLOR_F = rgba(0, 0, 0, 0.30);
pub const PIN_FRAME: D2D1_COLOR_F = rgba(255, 255, 255, 0.30);
pub const TRANSPARENT: D2D1_COLOR_F = rgba(0, 0, 0, 0.0);
/// 録画の赤: 録画枠の芯と停止のボタンだけ。
pub const REC: D2D1_COLOR_F = hex(0xD93A2B);
pub const REC_HOVER: D2D1_COLOR_F = hex(0xC4301F);
pub const ON_REC: D2D1_COLOR_F = hex(0xFFFFFF);

#[derive(Clone, Copy)]
pub struct Palette {
    pub text_primary: D2D1_COLOR_F,
    pub text_secondary: D2D1_COLOR_F,
    pub text_disabled: D2D1_COLOR_F,
    pub on_fill: D2D1_COLOR_F,
    pub on_fill_hover: D2D1_COLOR_F,
    pub on_fill_pressed: D2D1_COLOR_F,
    pub on_content: D2D1_COLOR_F,
    pub fill_hover: D2D1_COLOR_F,
    pub fill_pressed: D2D1_COLOR_F,
    pub separator: D2D1_COLOR_F,
    pub track: D2D1_COLOR_F,
    pub surface_veil: D2D1_COLOR_F,
    pub surface_opaque: D2D1_COLOR_F,
    /// 背後のぼかし (accent) の上に塗る HUD の地。ぼかしが無い窓は `surface_opaque`。
    pub surface_acrylic: D2D1_COLOR_F,
    pub focus_ring: D2D1_COLOR_F,
    pub focus_gap: D2D1_COLOR_F,
    pub border_swatch: D2D1_COLOR_F,
    pub fill_subtle: D2D1_COLOR_F,
    pub hairline: D2D1_COLOR_F,
    pub control_fill: D2D1_COLOR_F,
    pub control_fill_hover: D2D1_COLOR_F,
    pub control_stroke: D2D1_COLOR_F,
    pub danger: D2D1_COLOR_F,
    pub danger_wash: D2D1_COLOR_F,
    pub dim_out: D2D1_COLOR_F,
    pub select_ring: D2D1_COLOR_F,
    pub select_wash: D2D1_COLOR_F,
    pub border_strong: D2D1_COLOR_F,
    /// 全体バーの選んだ範囲。画像の上の帯はテーマに関係なく DARK の値、注釈モードでトレイに付けた帯はトレイの値。
    pub selection: D2D1_COLOR_F,
    pub control_fill_active: D2D1_COLOR_F,
    pub keycap: D2D1_COLOR_F,
}

pub const DARK: Palette = Palette {
    text_primary: hex(0xFFFFFF),
    text_secondary: rgba(255, 255, 255, 0.66),
    text_disabled: rgba(255, 255, 255, 0.36),
    on_fill: hex(0xFFFFFF),
    on_fill_hover: hex(0xE4E4E8),
    on_fill_pressed: hex(0xC9C9CF),
    on_content: hex(0x111114),
    fill_hover: rgba(255, 255, 255, 0.10),
    fill_pressed: rgba(255, 255, 255, 0.17),
    separator: rgba(255, 255, 255, 0.18),
    track: rgba(255, 255, 255, 0.24),
    surface_veil: rgba(10, 10, 12, 0.90),
    surface_opaque: hex(0x222226),
    surface_acrylic: rgba(30, 30, 34, 0.90),
    focus_ring: hex(0xFFFFFF),
    focus_gap: rgba(0, 0, 0, 0.70),
    border_swatch: rgba(255, 255, 255, 0.38),
    fill_subtle: rgba(255, 255, 255, 0.06),
    hairline: rgba(255, 255, 255, 0.09),
    control_fill: rgba(255, 255, 255, 0.07),
    control_fill_hover: rgba(255, 255, 255, 0.11),
    control_stroke: rgba(255, 255, 255, 0.08),
    danger: hex(0xFF7A6B),
    danger_wash: rgba(255, 122, 107, 0.20),
    dim_out: rgba(0, 0, 0, 0.60),
    select_ring: rgba(255, 255, 255, 0.75),
    select_wash: rgba(255, 255, 255, 0.10),
    border_strong: rgba(255, 255, 255, 0.62),
    selection: rgba(255, 255, 255, 0.62),
    control_fill_active: rgba(14, 14, 16, 0.70),
    keycap: rgba(255, 255, 255, 0.12),
};

pub const LIGHT: Palette = Palette {
    text_primary: hex(0x111114),
    text_secondary: rgba(17, 17, 20, 0.64),
    text_disabled: rgba(17, 17, 20, 0.36),
    on_fill: hex(0x111114),
    on_fill_hover: hex(0x2C2C31),
    on_fill_pressed: hex(0x46464E),
    on_content: hex(0xFFFFFF),
    fill_hover: rgba(0, 0, 0, 0.06),
    fill_pressed: rgba(0, 0, 0, 0.10),
    separator: rgba(0, 0, 0, 0.16),
    track: rgba(17, 17, 20, 0.16),
    surface_veil: rgba(250, 250, 251, 0.92),
    surface_opaque: hex(0xF3F3F5),
    surface_acrylic: rgba(246, 246, 248, 0.88),
    focus_ring: hex(0x111114),
    focus_gap: hex(0xFFFFFF),
    border_swatch: rgba(0, 0, 0, 0.26),
    fill_subtle: rgba(0, 0, 0, 0.04),
    hairline: rgba(0, 0, 0, 0.09),
    control_fill: rgba(255, 255, 255, 0.70),
    control_fill_hover: rgba(255, 255, 255, 0.50),
    control_stroke: rgba(0, 0, 0, 0.08),
    danger: hex(0xC6432F),
    danger_wash: rgba(198, 67, 47, 0.14),
    dim_out: rgba(255, 255, 255, 0.66),
    select_ring: rgba(17, 17, 20, 0.75),
    select_wash: rgba(17, 17, 20, 0.07),
    border_strong: rgba(17, 17, 20, 0.56),
    selection: rgba(17, 17, 20, 0.48),
    control_fill_active: hex(0xFFFFFF),
    keycap: rgba(0, 0, 0, 0.07),
};

pub fn palette() -> &'static Palette {
    if apps_dark() { &DARK } else { &LIGHT }
}

// ---- 書体 ----

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Style {
    Caption,
    CaptionStrong,
    Body,
    BodyStrong,
    /// `body` を tabular figures で組む (ホットキー記録欄・ドロップダウンの値・一覧の行)。
    BodyTabular,
    /// `body-strong` を tabular figures で組む (一覧の選んだ行)。
    BodyStrongTabular,
    Num,
    NumStrong,
    NumSmall,
    /// フィルムストリップの今のフレームの番号 (600)。
    NumSmallStrong,
}

impl Style {
    /// (文字サイズ, 行の高さ, 太字, tabular figures)
    fn spec(self) -> (f32, f32, bool, bool) {
        match self {
            Style::Caption => (12.0, 16.0, false, false),
            Style::CaptionStrong => (12.0, 16.0, true, false),
            Style::Body => (14.0, 20.0, false, false),
            Style::BodyStrong => (14.0, 20.0, true, false),
            Style::BodyTabular => (14.0, 20.0, false, true),
            Style::BodyStrongTabular => (14.0, 20.0, true, true),
            Style::Num => (12.0, 16.0, false, true),
            Style::NumStrong => (12.0, 16.0, true, true),
            Style::NumSmall => (10.0, 14.0, false, true),
            Style::NumSmallStrong => (10.0, 14.0, true, true),
        }
    }
}

/// Segoe UI Variable の光学サイズ: 12 以下は Small、20 以上は Display、間は Text。
fn family(size: f32) -> PCWSTR {
    if size <= 12.0 {
        w!("Segoe UI Variable Small")
    } else if size >= 20.0 {
        w!("Segoe UI Variable Display")
    } else {
        w!("Segoe UI Variable Text")
    }
}

/// 組んだ 1 行。幅と高さは物理 px。
pub struct Text {
    layout: IDWriteTextLayout,
    /// 組んだ字 (UTF-16)。DirectWrite の位置はこの添字。
    wide: Vec<u16>,
    pub w: f32,
    pub h: f32,
}

pub fn text(s_: &str, style: Style, s: f32) -> Text {
    text_in(s_, style, s, None)
}

/// 幅 `max_w` (物理 px) で折り返す段落。高さは行数 × 行の高さ。
pub fn text_wrapped(s_: &str, style: Style, s: f32, max_w: f32) -> Text {
    text_in(s_, style, s, Some(max_w))
}

fn text_in(s_: &str, style: Style, s: f32, wrap: Option<f32>) -> Text {
    let (size, lh, bold, tabular) = style.spec();
    let g = gfx();
    let wide: Vec<u16> = s_.encode_utf16().collect();
    unsafe {
        let key = (style, s.to_bits(), wrap.is_some());
        let format = FORMATS.with(|f| {
            f.borrow_mut()
                .entry(key)
                .or_insert_with(|| {
                    let format = g
                        .dwrite
                        .CreateTextFormat(
                            family(size),
                            None,
                            if bold {
                                DWRITE_FONT_WEIGHT_SEMI_BOLD
                            } else {
                                DWRITE_FONT_WEIGHT_NORMAL
                            },
                            DWRITE_FONT_STYLE_NORMAL,
                            DWRITE_FONT_STRETCH_NORMAL,
                            size * s,
                            w!("ja-jp"),
                        )
                        .expect("CreateTextFormat");
                    let _ = format.SetWordWrapping(if wrap.is_some() {
                        DWRITE_WORD_WRAPPING_WRAP
                    } else {
                        DWRITE_WORD_WRAPPING_NO_WRAP
                    });
                    // 行の高さを段の値に固定し、ベースラインを行の 3/4 に置く (字面が行の中央に来る)。
                    let _ = format.SetLineSpacing(
                        DWRITE_LINE_SPACING_METHOD_UNIFORM,
                        lh * s,
                        lh * s * 0.75,
                    );
                    if let Ok(f1) = format.cast::<IDWriteTextFormat1>() {
                        let _ = f1.SetFontFallback(&g.fallback);
                    }
                    format
                })
                .clone()
        });
        let layout = g
            .dwrite
            .CreateTextLayout(&wide, &format, wrap.unwrap_or(10000.0), lh * s)
            .expect("CreateTextLayout");
        if tabular {
            set_tabular(g, &layout, wide.len());
        }
        let mut m = DWRITE_TEXT_METRICS::default();
        let _ = layout.GetMetrics(&mut m);
        Text {
            layout,
            wide,
            w: m.widthIncludingTrailingWhitespace.ceil(),
            h: lh * s * m.lineCount.max(1) as f32,
        }
    }
}

/// 注釈の文字 (`size` は物理 px): Segoe UI Variable Text の Semibold、和文は Yu Gothic UI Bold。
/// 行の高さは書体のまま。
pub fn ink_text(s_: &str, size: f32) -> Text {
    free_text(s_, w!("Segoe UI Variable Text"), size, true, false)
}

/// 番号バッジの数字: Segoe UI Variable の 600、tabular figures。
pub fn badge_text(s_: &str, size: f32) -> Text {
    free_text(s_, family(size), size, false, true)
}

fn free_text(s_: &str, fam: PCWSTR, size: f32, cjk_bold: bool, tabular: bool) -> Text {
    let g = gfx();
    let wide: Vec<u16> = s_.encode_utf16().collect();
    unsafe {
        let format = g
            .dwrite
            .CreateTextFormat(
                fam,
                None,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size.max(1.0),
                w!("ja-jp"),
            )
            .expect("CreateTextFormat");
        let _ = format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
        if let Ok(f1) = format.cast::<IDWriteTextFormat1>() {
            let _ = f1.SetFontFallback(&g.fallback);
        }
        let layout = g
            .dwrite
            .CreateTextLayout(&wide, &format, 100000.0, 100000.0)
            .expect("CreateTextLayout");
        if cjk_bold {
            for (i, c) in wide.iter().enumerate() {
                if matches!(c, 0x3000..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xFF00..=0xFFEF)
                {
                    let _ = layout.SetFontWeight(
                        DWRITE_FONT_WEIGHT_BOLD,
                        DWRITE_TEXT_RANGE {
                            startPosition: i as u32,
                            length: 1,
                        },
                    );
                }
            }
        }
        if tabular {
            set_tabular(g, &layout, wide.len());
        }
        let mut m = DWRITE_TEXT_METRICS::default();
        let _ = layout.GetMetrics(&mut m);
        Text {
            layout,
            wide,
            w: m.widthIncludingTrailingWhitespace.ceil(),
            h: m.height.ceil(),
        }
    }
}

impl Text {
    /// 字数 (UTF-16)。
    pub fn len(&self) -> u32 {
        self.wide.len() as u32
    }

    /// 点 (行の箱の左上から) に一番近い字の境目 (UTF-16)。箱の外なら端に寄せる。
    pub fn pos_at(&self, x: f32, y: f32) -> u32 {
        let (mut trailing, mut inside) = (Default::default(), Default::default());
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        unsafe {
            let _ = self
                .layout
                .HitTestPoint(x, y, &mut trailing, &mut inside, &mut m);
        }
        let p = m.textPosition + if trailing.as_bool() { m.length } else { 0 };
        p.min(self.len())
    }

    /// `a..b` (UTF-16) を覆う矩形 (行ごと)。左上 (x, y) に描いたときの座標。
    pub fn range_rects(&self, a: u32, b: u32, x: f32, y: f32) -> Vec<D2D_RECT_F> {
        let (a, b) = (a.min(self.len()), b.min(self.len()));
        if a >= b {
            return Vec::new();
        }
        let mut n = 0;
        unsafe {
            let _ = self.layout.HitTestTextRange(a, b - a, x, y, None, &mut n);
            let mut m = vec![DWRITE_HIT_TEST_METRICS::default(); n as usize];
            if self
                .layout
                .HitTestTextRange(a, b - a, x, y, Some(&mut m), &mut n)
                .is_err()
            {
                return Vec::new();
            }
            m.truncate(n as usize);
            m.iter()
                .map(|m| rectf(m.left, m.top, m.left + m.width, m.top + m.height))
                .collect()
        }
    }

    /// `a..b` (UTF-16) の字。
    pub fn slice(&self, a: u32, b: u32) -> String {
        let (a, b) = (a.min(self.len()) as usize, b.min(self.len()) as usize);
        String::from_utf16_lossy(&self.wide[a.min(b)..b])
    }

    /// `i` 文字目 (UTF-16) の前の x。行の箱の左から。
    pub fn caret_x(&self, i: u32) -> f32 {
        let (mut x, mut y) = (0.0, 0.0);
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        unsafe {
            let _ = self
                .layout
                .HitTestTextPosition(i, false, &mut x, &mut y, &mut m);
        }
        x
    }
}

/// (x, y) は行の箱の左上。
pub fn draw_text(ctx: &ID2D1DeviceContext, t: &Text, x: f32, y: f32, color: D2D1_COLOR_F) {
    unsafe {
        let brush = ctx.CreateSolidColorBrush(&color, None).expect("brush");
        ctx.DrawTextLayout(
            windows_numerics::Vector2 { X: x, Y: y },
            &t.layout,
            &brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
        );
    }
}

/// 字形の輪郭を 1 つの形にする ((x, y) は行の箱の左上)。注釈のテキストの縁取りに使う。
pub fn text_outline(t: &Text, x: f32, y: f32) -> Option<ID2D1GeometryGroup> {
    use std::ffi::c_void;
    let mut runs: Vec<Option<ID2D1Geometry>> = Vec::new();
    // 受け手は呼び出しの間だけ生きる (DirectWrite は `Draw` の外で持たない)。参照の数は数えない。
    let mut obj: *const IDWriteTextRenderer_Vtbl = &OUTLINES;
    unsafe {
        let renderer = std::mem::ManuallyDrop::new(IDWriteTextRenderer::from_raw(
            &mut obj as *mut _ as *mut c_void,
        ));
        t.layout
            .Draw(Some(&mut runs as *mut _ as *const c_void), &*renderer, x, y)
            .ok()?;
        gfx()
            .factory
            .CreateGeometryGroup(D2D1_FILL_MODE_WINDING, &runs)
            .ok()
    }
}

/// `IDWriteTextLayout::Draw` の受け手の表: グリフの並びごとに輪郭の形を作り、描画の文脈
/// (`Vec<Option<ID2D1Geometry>>` を指す) へ足す。DirectWrite に縁取りは無いので輪郭から描く。
/// ponytail: `#[implement]` は `windows-core` を直接の依存に要るので、表を手で組む。
static OUTLINES: IDWriteTextRenderer_Vtbl = {
    use std::ffi::c_void;
    use windows::core::{BOOL, GUID, HRESULT, IUnknown, Interface};
    unsafe extern "system" fn query(
        this: *mut c_void,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        unsafe {
            let iid = *iid;
            if iid == IUnknown::IID
                || iid == IDWritePixelSnapping::IID
                || iid == IDWriteTextRenderer::IID
            {
                *out = this;
                HRESULT(0)
            } else {
                *out = std::ptr::null_mut();
                windows::Win32::Foundation::E_NOINTERFACE
            }
        }
    }
    unsafe extern "system" fn count(_: *mut c_void) -> u32 {
        1
    }
    unsafe extern "system" fn no_snap(_: *mut c_void, _: *const c_void, out: *mut BOOL) -> HRESULT {
        unsafe { *out = true.into() };
        HRESULT(0)
    }
    unsafe extern "system" fn transform(
        _: *mut c_void,
        _: *const c_void,
        m: *mut DWRITE_MATRIX,
    ) -> HRESULT {
        unsafe {
            *m = DWRITE_MATRIX {
                m11: 1.0,
                m22: 1.0,
                ..Default::default()
            }
        };
        HRESULT(0)
    }
    unsafe extern "system" fn per_dip(_: *mut c_void, _: *const c_void, out: *mut f32) -> HRESULT {
        unsafe { *out = 1.0 };
        HRESULT(0)
    }
    unsafe extern "system" fn glyphs(
        _: *mut c_void,
        runs: *const c_void,
        x: f32,
        y: f32,
        _: DWRITE_MEASURING_MODE,
        run: *const DWRITE_GLYPH_RUN,
        _: *const DWRITE_GLYPH_RUN_DESCRIPTION,
        _: *mut c_void,
    ) -> HRESULT {
        let add = || -> windows::core::Result<()> {
            unsafe {
                let run = &*run;
                let Some(face) = run.fontFace.as_ref() else {
                    return Ok(());
                };
                let factory = &gfx().factory;
                let geo = factory.CreatePathGeometry()?;
                let sink = geo.Open()?;
                sink.SetFillMode(D2D1_FILL_MODE_WINDING);
                face.GetGlyphRunOutline(
                    run.fontEmSize,
                    run.glyphIndices,
                    Some(run.glyphAdvances),
                    Some(run.glyphOffsets),
                    run.glyphCount,
                    run.isSideways.as_bool(),
                    run.bidiLevel & 1 == 1,
                    &sink,
                )?;
                sink.Close()?;
                let at = factory.CreateTransformedGeometry(
                    &geo,
                    &windows_numerics::Matrix3x2::translation(x, y),
                )?;
                (*(runs as *mut Vec<Option<ID2D1Geometry>>)).push(Some(at.into()));
                Ok(())
            }
        };
        add().map_or_else(|e| e.code(), |_| HRESULT(0))
    }
    unsafe extern "system" fn line<T>(
        _: *mut c_void,
        _: *const c_void,
        _: f32,
        _: f32,
        _: *const T,
        _: *mut c_void,
    ) -> HRESULT {
        HRESULT(0)
    }
    unsafe extern "system" fn inline(
        _: *mut c_void,
        _: *const c_void,
        _: f32,
        _: f32,
        _: *mut c_void,
        _: BOOL,
        _: BOOL,
        _: *mut c_void,
    ) -> HRESULT {
        HRESULT(0)
    }
    IDWriteTextRenderer_Vtbl {
        base__: IDWritePixelSnapping_Vtbl {
            base__: windows::core::IUnknown_Vtbl {
                QueryInterface: query,
                AddRef: count,
                Release: count,
            },
            IsPixelSnappingDisabled: no_snap,
            GetCurrentTransform: transform,
            GetPixelsPerDip: per_dip,
        },
        DrawGlyphRun: glyphs,
        DrawUnderline: line::<DWRITE_UNDERLINE>,
        DrawStrikethrough: line::<DWRITE_STRIKETHROUGH>,
        DrawInlineObject: inline,
    }
};

// ---- Win32 の小物 ----

pub fn contains(r: &RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

/// 左上 `at`、大きさ `w` × `h` の矩形。
pub fn rect_at(at: POINT, w: i32, h: i32) -> RECT {
    RECT {
        left: at.x,
        top: at.y,
        right: at.x + w,
        bottom: at.y + h,
    }
}

/// `d` だけ四方へ広げる (負なら縮める)。
pub fn inflate(r: RECT, d: i32) -> RECT {
    RECT {
        left: r.left - d,
        top: r.top - d,
        right: r.right + d,
        bottom: r.bottom + d,
    }
}

pub fn cursor() -> POINT {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    pt
}

/// カーソルの下に、`own` 以外のこのアプリの窓 (別のピン・別のツールバー) が出ているか。
/// 隠れたツールバーを場所だけで出す見回りが、重なったピンのツールバー同士で上を奪い合わないように使う。
pub fn ours_above(own: HWND) -> bool {
    unsafe {
        let top = GetAncestor(WindowFromPoint(cursor()), GA_ROOT);
        if top.is_invalid() || top == own {
            return false;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(top, Some(&mut pid));
        pid == std::process::id()
    }
}

/// `a` と `b` を囲む矩形。
pub fn union(a: RECT, b: RECT) -> RECT {
    RECT {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

/// 窓を最前面に置く / 外す (動かさず、大きさを変えず、アクティブにしない)。
pub fn set_topmost(hwnd: HWND, on: bool) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(if on { HWND_TOPMOST } else { HWND_NOTOPMOST }),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

pub fn work_area(r: RECT) -> RECT {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        let m = MonitorFromRect(&r, MONITOR_DEFAULTTONEAREST);
        let _ = GetMonitorInfoW(m, &mut info);
    }
    info.rcWork
}

/// 点のモニタ (`MONITOR_DEFAULTTONEAREST`)。DPI が取れなければ 96。
pub struct Monitor {
    pub handle: HMONITOR,
    pub rect: RECT,
    pub work: RECT,
    pub dpi: u32,
}

pub fn monitor_at(p: POINT) -> Monitor {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let (mut x, mut y) = (0, 0);
    unsafe {
        let handle = MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST);
        let _ = GetMonitorInfoW(handle, &mut info);
        let ok = GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_ok();
        Monitor {
            handle,
            rect: info.rcMonitor,
            work: info.rcWork,
            dpi: if ok && x > 0 { x } else { 96 },
        }
    }
}

pub fn lparam_point(lparam: LPARAM) -> POINT {
    POINT {
        x: (lparam.0 & 0xFFFF) as i16 as i32,
        y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
    }
}

pub fn track_leave(hwnd: HWND) {
    unsafe {
        let _ = TrackMouseEvent(&mut TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        });
    }
}

/// `DWMWA_EXTENDED_FRAME_BOUNDS`。取れなければ `GetWindowRect`。
pub fn frame_bounds(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    unsafe {
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut r).cast(),
            size_of::<RECT>() as u32,
        )
        .is_err()
        {
            let _ = GetWindowRect(hwnd, &mut r);
        }
    }
    r
}

// ---- 描画の小物 ----

pub fn rf(r: RECT) -> D2D_RECT_F {
    rectf(r.left as f32, r.top as f32, r.right as f32, r.bottom as f32)
}

pub fn rectf(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}

pub fn fill_rect(ctx: &ID2D1DeviceContext, r: D2D_RECT_F, color: D2D1_COLOR_F) {
    unsafe {
        let brush = ctx.CreateSolidColorBrush(&color, None).expect("brush");
        ctx.FillRectangle(&r, &brush);
    }
}

pub fn fill_round(ctx: &ID2D1DeviceContext, r: D2D_RECT_F, radius: f32, color: D2D1_COLOR_F) {
    unsafe {
        let brush = ctx.CreateSolidColorBrush(&color, None).expect("brush");
        ctx.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: r,
                radiusX: radius,
                radiusY: radius,
            },
            &brush,
        );
    }
}

/// 線の芯が `r` の辺に乗る矩形の線。
pub fn stroke_rect(ctx: &ID2D1DeviceContext, r: D2D_RECT_F, width: f32, color: D2D1_COLOR_F) {
    unsafe {
        let brush = ctx.CreateSolidColorBrush(&color, None).expect("brush");
        ctx.DrawRectangle(&r, &brush, width, None);
    }
}

pub fn identity() -> windows_numerics::Matrix3x2 {
    windows_numerics::Matrix3x2::identity()
}

/// `draw` で描いた形の影を、標準偏差 `std_dev` (トークンの blur 半径の半分) と `offset` で描く。
/// `clip_out` があれば、その矩形の内側には影を落とさない (画像や対象の画素を暗くしない)。
pub fn shadow(
    ctx: &ID2D1DeviceContext,
    std_dev: f32,
    color: D2D1_COLOR_F,
    offset: (f32, f32),
    clip_out: Option<D2D_RECT_F>,
    draw: impl FnOnce(&ID2D1DeviceContext),
) {
    unsafe {
        let Ok(list) = ctx.CreateCommandList() else {
            return;
        };
        let Ok(target) = ctx.GetTarget() else { return };
        ctx.SetTarget(&list);
        draw(ctx);
        ctx.SetTarget(&target);
        let _ = list.Close();
        let Ok(effect) = ctx.CreateEffect(&CLSID_D2D1Shadow) else {
            return;
        };
        effect.SetInput(0, &list, true);
        let _ = effect.SetValue(
            D2D1_SHADOW_PROP_BLUR_STANDARD_DEVIATION.0 as u32,
            D2D1_PROPERTY_TYPE_FLOAT,
            &std_dev.to_le_bytes(),
        );
        let c = [color.r, color.g, color.b, color.a];
        let bytes: Vec<u8> = c.iter().flat_map(|v| v.to_le_bytes()).collect();
        let _ = effect.SetValue(
            D2D1_SHADOW_PROP_COLOR.0 as u32,
            D2D1_PROPERTY_TYPE_VECTOR4,
            &bytes,
        );
        let Ok(image) = effect.cast::<ID2D1Image>() else {
            return;
        };
        let at = windows_numerics::Vector2 {
            X: offset.0,
            Y: offset.1,
        };
        match clip_out {
            None => ctx.DrawImage(
                &image,
                Some(&at),
                None,
                D2D1_INTERPOLATION_MODE_LINEAR,
                D2D1_COMPOSITE_MODE_SOURCE_OVER,
            ),
            Some(r) => outside(ctx, r, || {
                ctx.DrawImage(
                    &image,
                    Some(&at),
                    None,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    D2D1_COMPOSITE_MODE_SOURCE_OVER,
                )
            }),
        }
    }
}

/// `r` の外の 4 本の帯にだけ `draw` を描く (帯ごとに 1 回呼ぶ)。
fn outside(ctx: &ID2D1DeviceContext, r: D2D_RECT_F, draw: impl Fn()) {
    const BIG: f32 = 1.0e6;
    for band in [
        rectf(-BIG, -BIG, BIG, r.top),
        rectf(-BIG, r.bottom, BIG, BIG),
        rectf(-BIG, r.top, r.left, r.bottom),
        rectf(r.right, r.top, BIG, r.bottom),
    ] {
        unsafe {
            ctx.PushAxisAlignedClip(&band, D2D1_ANTIALIAS_MODE_ALIASED);
            draw();
            ctx.PopAxisAlignedClip();
        }
    }
}

/// ピンの影を描き置いた bitmap。鍵は (画像の幅, 高さ, 標準偏差のビット)。
pub struct ShadowBitmap {
    key: (i32, i32, u32),
    bitmap: ID2D1Bitmap1,
    /// bitmap の (margin, margin) が画像の左上 (ぼかしの裾の余白)。
    margin: f32,
}

/// 画像の矩形 `image` の影を、`cache` の bitmap から `opacity` を掛けて描く。画像の内側には落とさない。
/// 鍵 (大きさ・標準偏差) が合わなければ作り直す。`color` は呼び手ごとに決まった値 (鍵に入れない)。
/// 影は α が線形なので、不透明度は bitmap の `DrawBitmap` に掛ければ `color.a * opacity` で描いたのと同じ。
pub fn cached_shadow(
    ctx: &ID2D1DeviceContext,
    cache: &RefCell<Option<ShadowBitmap>>,
    image: D2D_RECT_F,
    std_dev: f32,
    color: D2D1_COLOR_F,
    offset: (f32, f32),
    opacity: f32,
) {
    let (w, h) = (
        (image.right - image.left) as i32,
        (image.bottom - image.top) as i32,
    );
    let key = (w, h, std_dev.to_bits());
    let mut cache = cache.borrow_mut();
    if cache.as_ref().map(|c| c.key) != Some(key) {
        *cache = shadow_bitmap(ctx, w, h, std_dev, color).map(|(bitmap, margin)| ShadowBitmap {
            key,
            bitmap,
            margin,
        });
    }
    let Some(c) = cache.as_ref() else { return };
    let (m, (dx, dy)) = (c.margin, offset);
    let dest = rectf(
        image.left - m + dx,
        image.top - m + dy,
        image.right + m + dx,
        image.bottom + m + dy,
    );
    outside(ctx, image, || unsafe {
        ctx.DrawBitmap(
            &c.bitmap,
            Some(&dest),
            opacity,
            D2D1_INTERPOLATION_MODE_LINEAR,
            None,
            None,
        )
    });
}

/// `w`×`h` の矩形の影 (offset なし) を、裾 (3 × 標準偏差) の余白付きの bitmap に 1 回描く。
/// 描画中 (`BeginDraw` の内側) に呼ぶ。返り値は (bitmap, 余白)。
fn shadow_bitmap(
    ctx: &ID2D1DeviceContext,
    w: i32,
    h: i32,
    std_dev: f32,
    color: D2D1_COLOR_F,
) -> Option<(ID2D1Bitmap1, f32)> {
    let m = (3.0 * std_dev).ceil() + 1.0;
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
        ..Default::default()
    };
    let size = D2D_SIZE_U {
        width: (w.max(1) as f32 + 2.0 * m) as u32,
        height: (h.max(1) as f32 + 2.0 * m) as u32,
    };
    unsafe {
        let bitmap = ctx.CreateBitmap(size, None, 0, &props).ok()?;
        let target = ctx.GetTarget().ok()?;
        let mut tf = identity();
        ctx.GetTransform(&mut tf);
        ctx.SetTransform(&identity());
        ctx.SetTarget(&bitmap);
        ctx.Clear(Some(&TRANSPARENT));
        shadow(ctx, std_dev, color, (0.0, 0.0), None, |c| {
            fill_rect(c, rectf(m, m, m + w as f32, m + h as f32), HL_DARK)
        });
        ctx.SetTarget(&target);
        ctx.SetTransform(&tf);
        Some((bitmap, m))
    }
}

// ---- DirectComposition の面 ----

/// 窓 1 枚ぶんの描画面。DirectComposition の swap chain (premultiplied alpha) に D2D で描く。
/// ピン・オーバーレイ・acrylic の窓が同じ物を使う (窓は `WS_EX_NOREDIRECTIONBITMAP`)。
pub struct Surface {
    swap: IDXGISwapChain1,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    /// 窓全体の不透明度 (`fade`)。
    effect: IDCompositionEffectGroup,
    pub ctx: ID2D1DeviceContext,
    pub w: u32,
    pub h: u32,
    /// 真なら `draw` は出さない (`present` まで待つ)。
    hold: Cell<bool>,
}

impl Surface {
    pub fn new(hwnd: HWND, w: u32, h: u32) -> Result<Self> {
        let g = gfx();
        let (w, h) = (w.max(1), h.max(1));
        unsafe {
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w,
                Height: h,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            let swap = g
                .dxgi_factory
                .CreateSwapChainForComposition(&g.d3d, &desc, None)?;
            let target = g.dcomp.CreateTargetForHwnd(hwnd, true)?;
            let visual = g.dcomp.CreateVisual()?;
            visual.SetContent(&swap)?;
            let effect = g.dcomp.CreateEffectGroup()?;
            visual.SetEffect(&effect)?;
            target.SetRoot(&visual)?;
            g.dcomp.Commit()?;
            let ctx = g
                .d2d
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            ctx.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let surface = Self {
                swap,
                _target: target,
                _visual: visual,
                effect,
                ctx,
                w,
                h,
                hold: Cell::new(false),
            };
            surface.bind()?;
            Ok(surface)
        }
    }

    fn bind(&self) -> Result<()> {
        unsafe {
            let buffer: IDXGISurface = self.swap.GetBuffer(0)?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                ..Default::default()
            };
            let bitmap = self
                .ctx
                .CreateBitmapFromDxgiSurface(&buffer, Some(&props))?;
            self.ctx.SetTarget(&bitmap);
        }
        Ok(())
    }

    pub fn resize(&mut self, w: u32, h: u32) -> Result<()> {
        let (w, h) = (w.max(1), h.max(1));
        if (w, h) == (self.w, self.h) {
            return Ok(());
        }
        unsafe {
            self.ctx.SetTarget(None);
            if let Err(e) =
                self.swap
                    .ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))
            {
                // 失敗しても古いバッファは残る。描き先を戻し、w/h も古いまま保つ。
                let _ = self.bind();
                return Err(e);
            }
        }
        self.w = w;
        self.h = h;
        self.bind()
    }

    /// 窓全体の不透明度を `from` から `to` へ `ms` で ease-out に動かす。動かすのは DWM (UI スレッドは描かない)。
    /// `ms` が 0 なら即座に `to`。
    pub fn fade(&self, from: f32, to: f32, ms: f32) {
        let g = gfx();
        unsafe {
            let set = || -> Result<()> {
                if ms <= 0.0 {
                    return self.effect.SetOpacity2(to);
                }
                // 1 - (1 - u)^3 = 3u - 3u² + u³ (u = t / T) を t の 3 次式にする。
                let (d, t) = (to - from, ms / 1000.0);
                let a = g.dcomp.CreateAnimation()?;
                a.AddCubic(0.0, from, 3.0 * d / t, -3.0 * d / (t * t), d / (t * t * t))?;
                a.End(t as f64, to)?;
                self.effect.SetOpacity(&a)
            };
            if let Err(e) = set() {
                eprintln!("surface: fade failed: {e}");
                let _ = self.effect.SetOpacity2(to);
            }
            let _ = g.dcomp.Commit();
        }
    }

    /// 次の `draw` は描くだけにして、出すのは `present` に任せる。窓の移動と新しい中身を同じコマに載せるのに使う
    /// (先に動かすと、描き終えるまで DWM が新しい位置に古い中身を出す)。
    pub fn hold(&self) {
        self.hold.set(true);
    }

    /// `hold` で描いた後、窓を動かす直前に呼ぶ。GPU が描き終えるのとコマの頭 (`DwmFlush`) を待つ。
    /// 待たないと、窓の移動と `present` が別のコマに載り、ホイールの拡縮で古い中身が 1 コマ新しい位置に出る。
    /// ponytail: 同じコマに載るのは確率が上がるだけで保証は無い。まだずれるなら窓を動かさず中身側で拡縮する。
    pub fn align(&self) {
        let g = gfx();
        unsafe {
            // D2D は EndDraw で出し終えている (BeginDraw の外の Flush は面を掴んだままにし、ResizeBuffers が失敗した)。
            let desc = D3D11_QUERY_DESC {
                Query: D3D11_QUERY_EVENT,
                MiscFlags: 0,
            };
            let mut q = None;
            if g.d3d.CreateQuery(&desc, Some(&mut q)).is_ok()
                && let Some(q) = q
            {
                g.d3d_ctx.End(&q);
                let mut done = windows::core::BOOL(0);
                let t0 = std::time::Instant::now();
                // S_FALSE (まだ) も Ok になるので、終わったかは `done` で見る。
                while g
                    .d3d_ctx
                    .GetData(
                        &q,
                        Some((&mut done as *mut windows::core::BOOL).cast()),
                        size_of::<windows::core::BOOL>() as u32,
                        0,
                    )
                    .is_ok()
                    && !done.as_bool()
                    && t0.elapsed().as_millis() < 50
                {
                    std::thread::yield_now();
                }
            }
            let _ = windows::Win32::Graphics::Dwm::DwmFlush();
        }
    }

    /// `hold` で止めた描画を出す。
    pub fn present(&self) {
        self.hold.set(false);
        unsafe {
            let _ = self.swap.Present(0, DXGI_PRESENT(0));
        }
    }

    /// 透明で消してから `f` で描き、出す。
    pub fn draw(&self, f: impl FnOnce(&ID2D1DeviceContext)) {
        unsafe {
            self.ctx.BeginDraw();
            self.ctx.SetTransform(&identity());
            self.ctx.Clear(Some(&TRANSPARENT));
            f(&self.ctx);
            if let Err(e) = self.ctx.EndDraw(None, None) {
                eprintln!("draw: EndDraw failed: {e}");
            }
            if !self.hold.get() {
                crate::insight_scope!("surface_present");
                let _ = self.swap.Present(1, DXGI_PRESENT(0));
            }
        }
    }
}

/// 文字列全体に tabular figures (数字の幅を揃える) を掛ける。
fn set_tabular(g: &Gfx, layout: &IDWriteTextLayout, len: usize) {
    unsafe {
        if let Ok(typo) = g.dwrite.CreateTypography() {
            let _ = typo.AddFontFeature(DWRITE_FONT_FEATURE {
                nameTag: DWRITE_FONT_FEATURE_TAG_TABULAR_FIGURES,
                parameter: 1,
            });
            let _ = layout.SetTypography(
                &typo,
                DWRITE_TEXT_RANGE {
                    startPosition: 0,
                    length: len as u32,
                },
            );
        }
    }
}

/// BGRA (straight、α=255 前提) の画素から D2D の bitmap を作る。
pub fn bitmap_from_bgra(
    ctx: &ID2D1DeviceContext,
    w: u32,
    h: u32,
    bgra: &[u8],
) -> Result<ID2D1Bitmap1> {
    unsafe {
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        ctx.CreateBitmap(
            D2D_SIZE_U {
                width: w,
                height: h,
            },
            Some(bgra.as_ptr().cast()),
            w * 4,
            &props,
        )
    }
}

/// 同じ D3D device の BGRA texture (凍結画像、α は不定) を、GPU に置いたまま α を無視する
/// (不透明に描く) D2D の bitmap にする。texture は `D3D11_BIND_SHADER_RESOURCE` が要る。
pub fn bitmap_from_texture(
    ctx: &ID2D1DeviceContext,
    texture: &ID3D11Texture2D,
) -> Result<ID2D1Bitmap1> {
    unsafe {
        let surface: IDXGISurface = texture.cast()?;
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        ctx.CreateBitmapFromDxgiSurface(&surface, Some(&props))
    }
}

// ---- 窓の小物 ----

/// 設定 `show_in_capture`。起動時と切り替えで `set_show_in_capture` が置く。
static SHOW_IN_CAPTURE: AtomicBool = AtomicBool::new(false);
/// `exclude_from_capture` を通した窓。切り替えたとき、生きているものだけ掛け直す。
static FOLLOWERS: Mutex<Vec<isize>> = Mutex::new(Vec::new());

/// 自アプリの窓 (ピン・GIF ピンとその HUD など) を、設定 `show_in_capture` がオフならほかのキャプチャに写さない。
/// 設定を切り替えると、開いている窓にもすぐ効く。
pub fn exclude_from_capture(hwnd: HWND) {
    FOLLOWERS
        .lock()
        .expect("capture followers")
        .push(hwnd.0 as isize);
    set_affinity(hwnd, !SHOW_IN_CAPTURE.load(Ordering::Relaxed));
}

/// 設定に関係なく写さない。範囲選択の面・録画のバー・カウントダウンなど、撮る道具そのもの。
pub fn always_exclude_from_capture(hwnd: HWND) {
    // 閉じたピンの番号が使い回されていたら、切り替えで外れないように一覧から落とす。
    let h = hwnd.0 as isize;
    FOLLOWERS
        .lock()
        .expect("capture followers")
        .retain(|&x| x != h);
    set_affinity(hwnd, true);
}

/// 設定 `show_in_capture` を置き、`exclude_from_capture` を通した開いている窓に掛け直す。
pub fn set_show_in_capture(on: bool) {
    SHOW_IN_CAPTURE.store(on, Ordering::Relaxed);
    let mut list = FOLLOWERS.lock().expect("capture followers");
    // ponytail: 閉じた窓は IsWindow で落とす。番号が設定窓などに使い回されると、その窓にも掛かる。
    // 困ったら窓を閉じるとき一覧から外す。
    list.retain(|&h| unsafe { IsWindow(Some(HWND(h as _))).as_bool() });
    for &h in list.iter() {
        set_affinity(HWND(h as _), !on);
    }
}

/// debug ビルドは `ODILE_SHOW_IN_CAPTURE=1` で除外を外す (実機検証で自分の窓を撮るため)。
fn set_affinity(hwnd: HWND, exclude: bool) {
    if exclude && cfg!(debug_assertions) && std::env::var_os("ODILE_SHOW_IN_CAPTURE").is_some() {
        return;
    }
    let mode = if exclude {
        WDA_EXCLUDEFROMCAPTURE
    } else {
        WDA_NONE
    };
    if let Err(e) = unsafe { SetWindowDisplayAffinity(hwnd, mode) } {
        eprintln!("display affinity failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpi_rounding() {
        // 150% の例 (DS README): ツールバー 40→60、角丸 8→12、ボタン 32→48、線 1→2。
        assert_eq!(px(40.0, 1.5), 60);
        assert_eq!(px(8.0, 1.5), 12);
        assert_eq!(px(32.0, 1.5), 48);
        assert_eq!(stroke(1.0, 1.5), 2);
        // 0.5 は切り上げ: 1×1.5 = 1.5 → 2、5×1.25 = 6.25 → 6、2×1.25 = 2.5 → 3。
        assert_eq!(px(1.0, 1.5), 2);
        assert_eq!(px(5.0, 1.25), 6);
        assert_eq!(px(2.0, 1.25), 3);
        // 線は 1 を下回らない。round(1×0.5)=1 でも max で守る値として 0.4 を試す。
        assert_eq!(stroke(1.0, 0.4), 1);
        assert_eq!(px(1.0, 0.4), 0);
        assert_eq!(px(40.0, 1.0), 40);
    }
}
