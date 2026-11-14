//! 静止画ピン: キャプチャした位置に残る枠なしの窓 (DirectComposition の透過窓、物理 px の 1:1)。
//! ホバーで出るツールバー・ツールチップ・ポップアップは別のトップレベル窓 (HUD)。
//! ツールバーの「注釈」で編集モードに入り、注釈 (`annot`) を描く。編集の間はツールバーの中身を置き換える。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dwm::{
    DWMWA_FORCE_ICONIC_REPRESENTATION, DWMWA_HAS_ICONIC_BITMAP, DWMWA_TRANSITIONS_FORCEDISABLED,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmFlush, DwmSetIconicLivePreviewBitmap,
    DwmSetIconicThumbnail,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_DELETE, VK_ESCAPE,
    VK_MENU, VK_SHIFT,
};
use windows::Win32::UI::Shell::{ITaskbarList, TaskbarList};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use crate::annot::{self, Doc, Tool};
use crate::annot_edit::{self, Choice, Editor, HostRef};
use crate::capture::Image;
use crate::layout::{
    local_img, pin_margins, pin_window_rect, place_popup, place_toolbar, rescue_rect,
};
use crate::traymenu;
use crate::ui::widgets::status::{Band, Kind, Status};
use crate::ui::widgets::{
    self, Acrylic, BtnState, Capture, Item, Label, Look, Part, Tooltip, draw_toolbar, popup_size,
    toolbar_height, toolbar_hit, toolbar_layout,
};
use crate::ui::{
    self, Palette, Style, Surface, anim, contains, cursor, lparam_point, px, rectf, stroke,
    track_leave, work_area,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;

const PIN_CLASS: PCWSTR = w!("OdilePin");
const HUD_CLASS: PCWSTR = w!("OdileHud");

/// 左右反転・クリックを下の窓へ・最前面は右クリックのメニュー (`open_menu`) にある。
const ITEMS: [Item; 10] = [
    Item::Tool("copy"),
    Item::Tool("save"),
    Item::Sep,
    Item::Tool("annotate"),
    Item::Sep,
    Item::Tool("opacity"),
    Item::Wide,
    Item::Tool("restore"),
    Item::Sep,
    Item::Tool("close"),
];
const I_COPY: usize = 0;
const I_SAVE: usize = 1;
const I_ANNOTATE: usize = 3;
const I_OPACITY: usize = 5;
const I_ZOOM: usize = 6;
const I_RESTORE: usize = 7;
const I_CLOSE: usize = 9;

/// 拡大率のボタンの一覧 (%)。GIF ピンも使う。
pub const ZOOMS: [u32; 8] = [400, 300, 200, 150, 100, 75, 50, 25];

/// 拡大率のボタンの文字。
pub fn zoom_label(zoom: f32) -> String {
    format!("{}%", (zoom * 100.0).round())
}

/// 一覧で印を付ける行 (今の拡大率が一覧に無ければ None)。
pub fn zoom_sel(zoom: f32) -> Option<usize> {
    ZOOMS
        .iter()
        .position(|&v| v == (zoom * 100.0).round() as u32)
}

/// ホイールの 1 刻みで 1.25 倍。
pub fn wheel_zoom(zoom: f32, delta: i16) -> f32 {
    zoom * 1.25f32.powf(delta as f32 / WHEEL_DELTA as f32)
}

/// 拡縮の矩形: 画像 `w` × `h` を倍率 `zoom` で `img` に置いている。`want` 倍にし、`around` の点が画像の同じ位置を
/// 指すよう置き直す。→ (倍率, 矩形)。倍率の上限は 8 倍と、窓 (画像 + 余白、`s`) が D3D のテクスチャ上限に入る
/// 倍率の小さい方。下限は 0.1 倍。ただし今の倍率がそれより小さければ今の倍率 (縮小で大きくならない)。
/// 1.0 の ±0.01 は 1.0 に吸着する。静止画ピンと GIF ピンが使う。
/// `zoom` は横の倍率。縦の倍率は `zoom × aspect` (隅の自由な拡縮で 1 から外れる。`aspect`)。
/// 矩形の高さは `aspect` から計算するので、`img` の高さは `around` の位置を計算するときにしか使わない。
pub fn zoom_rect(
    (w, h): (u32, u32),
    zoom: f32,
    img: RECT,
    want: f32,
    around: POINT,
    s: f32,
    aspect: f32,
) -> (f32, RECT) {
    let (ml, mt, mr, mb) = pin_margins(s);
    let max = ((MAX_TEXTURE - (ml + mr) as f32) / w.max(1) as f32)
        .min((MAX_TEXTURE - (mt + mb) as f32) / (h.max(1) as f32 * aspect))
        .clamp(0.1, 8.0);
    let z = want.clamp(zoom.min(0.1), max);
    let z = if (z - 1.0).abs() < 0.01 { 1.0 } else { z };
    let (rw, rh) = (
        (w as f32 * z).round().max(1.0) as i32,
        (h as f32 * z * aspect).round().max(1.0) as i32,
    );
    // around が画像の同じ位置を指すよう置き直す。縦は縦横比が変わる分 (Ctrl+0 で 1 に戻す) も掛ける。
    let kx = z / zoom;
    let ky = kx * aspect / self::aspect(h, zoom, img);
    let pos = POINT {
        x: around.x - ((around.x - img.left) as f32 * kx).round() as i32,
        y: around.y - ((around.y - img.top) as f32 * ky).round() as i32,
    };
    (z, ui::rect_at(pos, rw, rh))
}

/// 画像の中心。縦横比を戻すときに止める点。
pub fn zoom_center(img: RECT) -> POINT {
    POINT {
        x: (img.left + img.right) / 2,
        y: (img.top + img.bottom) / 2,
    }
}

/// 縦の倍率 ÷ 横の倍率 (`zoom` は横の倍率)。高さが等倍の縦横比の高さから 1 px 以内なら 1.0
/// (丸めで比が少しずつずれないように)。
pub fn aspect(h: u32, zoom: f32, img: RECT) -> f32 {
    let ih = img.bottom - img.top;
    let even = (h as f32 * zoom).round().max(1.0);
    if (ih as f32 - even).abs() <= 1.0 {
        1.0
    } else {
        ih as f32 / (h.max(1) as f32 * zoom)
    }
}

thread_local! {
    /// 縁を最後に押した (時刻, 画面の点)。縁のダブルクリックを数える。
    static EDGE_DOWN: std::cell::Cell<(u32, POINT)> = const { std::cell::Cell::new((0, POINT { x: 0, y: 0 })) };
}

/// 縁を押した。前の押下とダブルクリックの間と距離に入っていれば true (数え直す: 3 回目は 1 回目に戻る)。
/// ピンの窓は `CS_DBLCLKS` を持たないので自分で数える。
pub fn edge_double(c: POINT) -> bool {
    let now = unsafe { GetMessageTime() } as u32;
    let (t0, c0) = EDGE_DOWN.with(|d| d.get());
    let dbl = now.wrapping_sub(t0) <= unsafe { GetDoubleClickTime() }
        && (c.x - c0.x).abs() * 2 <= unsafe { GetSystemMetrics(SM_CXDOUBLECLK) }
        && (c.y - c0.y).abs() * 2 <= unsafe { GetSystemMetrics(SM_CYDOUBLECLK) };
    EDGE_DOWN.with(|d| d.set(if dbl { (0, c) } else { (now, c) }));
    dbl
}

/// Ctrl を押しているか。
pub fn ctrl() -> bool {
    unsafe { GetKeyState(VK_CONTROL.0 as i32) < 0 }
}

/// Alt を押しているか。
pub fn alt() -> bool {
    unsafe { GetKeyState(VK_MENU.0 as i32) < 0 }
}

/// Shift を押しているか。
pub fn shift() -> bool {
    unsafe { GetKeyState(VK_SHIFT.0 as i32) < 0 }
}

/// 縁のドラッグの向き: (左右, 上下)。各 -1 (左・上) / 0 / 1 (右・下)。
pub type Edge = (i8, i8);

/// 縁の掴む帯の当たり: 画像 `img` の内側の縁から 6 × s。外 (影の余白) は掴まない (余白はクリックを下の窓へ通す)。
/// 小さい画像で縁が中を覆わないよう、帯は辺の 1/4 まで。画像の外と帯の外は None。
pub fn edge_hit(img: RECT, pt: POINT, s: f32) -> Option<Edge> {
    if !contains(&img, pt) {
        return None;
    }
    let b = px(6.0, s);
    let side = |v: i32, lo: i32, hi: i32| {
        let b = b.min((hi - lo) / 4);
        if v < lo + b {
            -1
        } else if v >= hi - b {
            1
        } else {
            0
        }
    };
    let e = (
        side(pt.x, img.left, img.right),
        side(pt.y, img.top, img.bottom),
    );
    (e != (0, 0)).then_some(e)
}

/// 縁の向きのカーソル。
pub fn edge_cursor(e: Edge) -> PCWSTR {
    match e {
        (0, _) => IDC_SIZENS,
        (_, 0) => IDC_SIZEWE,
        (x, y) if x == y => IDC_SIZENWSE,
        _ => IDC_SIZENESW,
    }
}

/// 縁のドラッグ: 押したときの向き・カーソル・画像の矩形・倍率。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Resize {
    pub edge: Edge,
    pub start: POINT,
    pub img: RECT,
    pub zoom: f32,
}

impl Resize {
    /// カーソル `c` での (横の倍率, 矩形)。引いた角の対角 (辺なら対辺の中点) を止める。
    /// 角と辺は引いた向きだけを伸ばす (辺は 1 軸、角は縦横を別に)。`keep` なら縦横比を保つ。
    /// 縦横比を保つときは、幅と高さの変化の大きい方に従う。上限・下限・1.0 への吸着は `zoom_rect` と同じ。
    pub fn at(&self, (w, h): (u32, u32), c: POINT, s: f32, keep: bool) -> (f32, RECT) {
        let r = self.img;
        let (rw, rh) = (r.right - r.left, r.bottom - r.top);
        if !keep {
            let (ml, mt, mr, mb) = pin_margins(s);
            // 1 軸の倍率: 押したときの倍率 `base` を、伸ばした長さの比で掛ける。
            // 引いていない軸 (辺のドラッグの他方) は倍率をそのまま残す。
            let axis = |n: u32, margin: i32, base: f32, len: i32, e: i8, d: i32| {
                if e == 0 {
                    return base;
                }
                let max = ((MAX_TEXTURE - margin as f32) / n.max(1) as f32).clamp(0.1, 8.0);
                let z = base * (len + e as i32 * d) as f32 / len.max(1) as f32;
                let z = z.clamp(base.min(0.1), max);
                if (z - 1.0).abs() < 0.01 { 1.0 } else { z }
            };
            let zx = axis(w, ml + mr, self.zoom, rw, self.edge.0, c.x - self.start.x);
            let zy0 = rh as f32 / h.max(1) as f32;
            let zy = axis(h, mt + mb, zy0, rh, self.edge.1, c.y - self.start.y);
            let len = |e: i8, n: u32, z: f32, now: i32| {
                if e == 0 {
                    now
                } else {
                    (n as f32 * z).round().max(1.0) as i32
                }
            };
            let (nw, nh) = (len(self.edge.0, w, zx, rw), len(self.edge.1, h, zy, rh));
            // 引いた辺の対辺を止める。引いていない軸はそのまま。
            let left = if self.edge.0 == -1 {
                r.right - nw
            } else {
                r.left
            };
            let top = if self.edge.1 == -1 {
                r.bottom - nh
            } else {
                r.top
            };
            return (zx, ui::rect_at(POINT { x: left, y: top }, nw, nh));
        }
        let ratio = |e: i8, d: i32, len: i32| {
            (e != 0).then(|| (len + e as i32 * d) as f32 / len.max(1) as f32)
        };
        let rx = ratio(self.edge.0, c.x - self.start.x, rw);
        let ry = ratio(self.edge.1, c.y - self.start.y, rh);
        let k = match (rx, ry) {
            (Some(a), Some(b)) if (a - 1.0).abs() >= (b - 1.0).abs() => a,
            (Some(_), Some(b)) | (None, Some(b)) => b,
            (Some(a), None) => a,
            (None, None) => 1.0,
        };
        let fixed = |e: i8, lo: i32, hi: i32| match e {
            1 => lo,
            -1 => hi,
            _ => (lo + hi) / 2,
        };
        let around = POINT {
            x: fixed(self.edge.0, r.left, r.right),
            y: fixed(self.edge.1, r.top, r.bottom),
        };
        let a = aspect(h, self.zoom, r);
        zoom_rect((w, h), self.zoom, r, self.zoom * k, around, s, a)
    }
}

/// ピンの窓 (最前面・DComp の透過窓) を `guess` に作る。角を丸めず、出入りのアニメーションを切り、
/// キャプチャから外す。静止画ピンと GIF ピンが使う。
/// ツール窓ではないので、1 枚ずつタスクバーと Alt+Tab に出る。`WS_MINIMIZEBOX` が無いと、タスクバーのボタンで
/// 最小化しない。`WS_CAPTION` は出した後に `show_now` が足す。サムネイルは iconic (`send_iconic`): キャプチャから外した窓の生の絵は空になるため。
pub fn create_window(class: PCWSTR, guess: RECT, tag: &str) -> windows::core::Result<HWND> {
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP,
            class,
            &HSTRING::from(crate::PRODUCT_NAME),
            WS_POPUP | WS_MINIMIZEBOX,
            guess.left,
            guess.top,
            guess.right - guess.left,
            guess.bottom - guess.top,
            None,
            None,
            GetModuleHandleW(None).ok().map(Into::into),
            None,
        )
    }
    .inspect_err(|e| eprintln!("{tag}: window failed: {e}"))?;
    let _ = widgets::set_attr(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_DONOTROUND);
    // DWM の遷移は切らない: 閉じる・最小化・復元は設定の窓と同じ OS の遷移で動く (ツールバーは `hide_now` で先に消す)。
    ui::exclude_from_capture(hwnd);
    let on = windows::core::BOOL::from(true);
    let _ = widgets::set_attr(hwnd, DWMWA_FORCE_ICONIC_REPRESENTATION, &on);
    let _ = widgets::set_attr(hwnd, DWMWA_HAS_ICONIC_BITMAP, &on);
    prevent_pinning(hwnd);
    Ok(hwnd)
}

/// ピンを今のコマで出し、それから `WS_CAPTION` を足す。`WS_CAPTION` が無いと DWM は閉じる・最小化・復元に
/// 遷移を付けない。出す前から付けると出るときもフェードになり、`DWMWA_TRANSITIONS_FORCEDISABLED` でも止まらない。
/// タイトルバーはピンの wndproc が `WM_NCCALCSIZE` で消す。
pub fn show_now(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        SetWindowLongPtrW(hwnd, GWL_STYLE, style | WS_CAPTION.0 as isize);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// タスクバーのボタンの右クリックメニューから「タスクバーにピン留めする」を消す
/// (`System.AppUserModel.PreventPinning`)。ピン窓と設定窓のボタンからアプリを留めさせない。
/// アプリ名の項目 (起動し直し) も一緒に消える。
pub fn prevent_pinning(hwnd: HWND) {
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::Foundation::VARIANT_TRUE;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Variant::VT_BOOL;
    use windows::Win32::UI::Shell::PropertiesSystem::{
        IPropertyStore, SHGetPropertyStoreForWindow,
    };
    // PKEY_AppUserModel_PreventPinning。定数は Win32_Storage_EnhancedStorage にあるが、この 1 つのために機能を足さない。
    const KEY: PROPERTYKEY = PROPERTYKEY {
        fmtid: windows::core::GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
        pid: 9,
    };
    let mut value = PROPVARIANT::default();
    unsafe {
        let inner = &mut *value.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = VARIANT_TRUE;
    }
    let result = unsafe {
        SHGetPropertyStoreForWindow::<IPropertyStore>(hwnd)
            .and_then(|store| store.SetValue(&KEY, &value).and_then(|()| store.Commit()))
    };
    if let Err(e) = result {
        eprintln!("pin: prevent pinning failed: {e}");
    }
}

/// タスクバーと Alt+Tab のサムネイル (`WM_DWMSENDICONICTHUMBNAIL`) とプレビュー
/// (`WM_DWMSENDICONICLIVEPREVIEWBITMAP`) に `image` を渡す。`win` は窓の大きさ、`at` は窓の中の画像の矩形。
/// どちらの知らせでもなければ false。
pub fn send_iconic(
    hwnd: HWND,
    msg: u32,
    lparam: LPARAM,
    image: &Image,
    win: (i32, i32),
    at: RECT,
) -> bool {
    let (w, h, dst) = match msg {
        WM_DWMSENDICONICTHUMBNAIL => {
            // HIWORD が幅の上限、LOWORD が高さの上限。縦横比を保って収める。
            let (mw, mh) = ((lparam.0 >> 16) & 0xFFFF, lparam.0 & 0xFFFF);
            let k = (mw as f32 / image.w as f32).min(mh as f32 / image.h as f32);
            let w = ((image.w as f32 * k) as i32).max(1);
            let h = ((image.h as f32 * k) as i32).max(1);
            (w, h, ui::rect_at(POINT::default(), w, h))
        }
        WM_DWMSENDICONICLIVEPREVIEWBITMAP => (win.0.max(1), win.1.max(1), at),
        _ => return false,
    };
    let pixels = iconic_pixels(image, (w, h), dst);
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    unsafe {
        let mut bits = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return true;
        };
        std::slice::from_raw_parts_mut(bits as *mut u8, pixels.len()).copy_from_slice(&pixels);
        let r = if msg == WM_DWMSENDICONICTHUMBNAIL {
            DwmSetIconicThumbnail(hwnd, bmp, 0)
        } else {
            DwmSetIconicLivePreviewBitmap(hwnd, bmp, None, 0)
        };
        if let Err(e) = r {
            eprintln!("pin: iconic {msg:#x} {w}x{h} failed: {e}");
        }
        let _ = DeleteObject(bmp.into());
    }
    true
}

/// `w`×`h` の上から下への BGRA (乗算済み) に、`image` を `dst` へ最近傍で拡縮して置く。`dst` の外は透明。
fn iconic_pixels(image: &Image, (w, h): (i32, i32), dst: RECT) -> Vec<u8> {
    let mut out = vec![0u8; (w * h * 4) as usize];
    blit(&mut out, (w, h), image, dst);
    out
}

/// `w`×`h` の上から下への BGRA の `out` に、`image` を `dst` へ最近傍で拡縮して不透明で置く。
fn blit(out: &mut [u8], (w, h): (i32, i32), image: &Image, dst: RECT) {
    let (dw, dh) = (
        (dst.right - dst.left).max(1) as usize,
        (dst.bottom - dst.top).max(1) as usize,
    );
    let (iw, ih) = (image.w as usize, image.h as usize);
    for y in dst.top.max(0)..dst.bottom.min(h) {
        let sy = (y - dst.top) as usize * ih / dh;
        for x in dst.left.max(0)..dst.right.min(w) {
            let s = (sy * iw + (x - dst.left) as usize * iw / dw) * 4;
            let d = ((y * w + x) * 4) as usize;
            out[d..d + 3].copy_from_slice(&image.bgra[s..s + 3]);
            out[d + 3] = 255;
        }
    }
}

/// 束のコピー・保存の絵: (画面の画像の矩形, 画像) を奥から順に並べた物を、画面の並びのまま 1 枚に重ねる。
/// 外接矩形の大きさで、どのピンも覆わない所は透明。
/// ponytail: 画面の 1 px を 1 px にし、最近傍で拡縮する。拡大したピンの元の解像度は捨てる。要るなら最大の倍率で描く。
fn compose(parts: &[(RECT, Image)]) -> Image {
    let bound = parts
        .iter()
        .map(|p| p.0)
        .reduce(ui::union)
        .unwrap_or_default();
    let (w, h) = (bound.right - bound.left, bound.bottom - bound.top);
    let mut out = vec![0u8; (w.max(0) * h.max(0) * 4) as usize];
    for (r, image) in parts {
        let dst = RECT {
            left: r.left - bound.left,
            top: r.top - bound.top,
            right: r.right - bound.left,
            bottom: r.bottom - bound.top,
        };
        blit(&mut out, (w, h), image, dst);
    }
    Image {
        w: w.max(0) as u32,
        h: h.max(0) as u32,
        bgra: out,
    }
}

/// 窓の Z 順の深さ (手前に何枚あるか)。
fn depth(hwnd: HWND) -> usize {
    let mut n = 0;
    let mut h = hwnd;
    while let Ok(prev) = unsafe { GetWindow(h, GW_HWNDPREV) } {
        h = prev;
        n += 1;
    }
    n
}

/// タスクバーのボタンを合わせ直す。ピンが開くとき・閉じたとき・束が変わったときに呼ぶ。
/// 題は「ピン {n} ({w} × {h})」(トレイと同じ文言)。静止画ピンはトレイと同じ番号 (開いた順)。GIF ピンはトレイに
/// 出ないので、静止画ピンの後に続けて振る。束は代表 (束で最初に開いたピン) だけがボタンを持ち、題は
/// 「束 {n} ({k} 枚)」。ほかのピンのボタンは `DeleteTab` で消す。1 枚になった束はここで消す。
pub fn retitle() {
    let t = crate::strings();
    let still = list().into_iter().map(|(hwnd, w, h, _)| (hwnd, w, h));
    let live: Vec<(HWND, u32, u32)> = still.chain(crate::gifpin::sizes()).collect();
    // 束の表を開いているピンに合わせる (閉じたピンを外し、新しいピンを開いた順に足す)。
    let (members, removed) = MEMBERS.with(|m| {
        let mut m = m.borrow_mut();
        let before = m.len();
        m.retain(|x| live.iter().any(|l| l.0 == x.hwnd));
        let removed = m.len() != before;
        for &(hwnd, ..) in &live {
            if !m.iter().any(|x| x.hwnd == hwnd) {
                m.push(Member {
                    hwnd,
                    group: None,
                    selected: false,
                    tab_off: false,
                });
            }
        }
        let groups: Vec<_> = m.iter().map(|x| x.group).collect();
        let plan = tabs(&groups);
        for (x, tab) in m.iter_mut().zip(&plan) {
            if *tab == Tab::Own && x.group.take().is_some() {
                eprintln!("group: dissolved (one pin left)");
            }
        }
        (m.iter().copied().zip(plan).collect::<Vec<_>>(), removed)
    });
    for (x, tab) in members {
        let name = match tab {
            Tab::Bundle { n, k } => t
                .taskbar_group
                .replace("{n}", &n.to_string())
                .replace("{k}", &k.to_string()),
            _ => {
                let Some(i) = live.iter().position(|l| l.0 == x.hwnd) else {
                    continue;
                };
                let (_, w, h) = live[i];
                t.tray_pin
                    .replace("{n}", &(i + 1).to_string())
                    .replace("{w}", &w.to_string())
                    .replace("{h}", &h.to_string())
            }
        };
        unsafe {
            let _ = SetWindowTextW(x.hwnd, &HSTRING::from(name));
        }
        let off = tab == Tab::Off;
        if off != x.tab_off {
            set_tab(x.hwnd, !off);
            MEMBERS.with(|m| {
                if let Some(y) = m.borrow_mut().iter_mut().find(|y| y.hwnd == x.hwnd) {
                    y.tab_off = off;
                }
            });
        }
    }
    // ピンを閉じると、選んだ枚数や束の枚数が変わる (「束ねる」と束のボタンが出入りする)。
    if removed {
        refresh_all();
    }
}

// ---- ピンの束 (DS `PinGroup`、docs/design.md「ピンのグループ」) ----
// 束は窓の表 (`MEMBERS`、静止画ピンと GIF ピンを開いた順に通す) に束の id を持たせて表す。
// 束の操作は「同じ id のピン全部に同じ操作」。プロセスの中だけに持つ (ピンも再起動で残らない)。

/// 束の表の 1 行。
#[derive(Clone, Copy)]
struct Member {
    hwnd: HWND,
    group: Option<u32>,
    selected: bool,
    /// タスクバーのボタンを `DeleteTab` で消している。
    tab_off: bool,
}

thread_local! {
    static MEMBERS: RefCell<Vec<Member>> = const { RefCell::new(Vec::new()) };
    static NEXT_GROUP: Cell<u32> = const { Cell::new(1) };
    /// カーソルが乗っているピンの束 (束の枠を出す)。
    static HOVER_GROUP: Cell<Option<u32>> = const { Cell::new(None) };
    static TASKBAR: RefCell<Option<ITaskbarList>> = const { RefCell::new(None) };
}

/// ピン 1 枚のタスクバーのボタン。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    /// ピンの題のボタン (束でない、または 1 枚だけの束)。
    Own,
    /// 束の代表: 題は「束 {n} ({k} 枚)」。`n` は束の番号 (代表の開いた順)。
    Bundle { n: usize, k: usize },
    /// 束の代表でないピン: ボタンを消す。
    Off,
}

/// 開いた順のピンの束の id の列から、各ピンのタスクバーのボタン。
fn tabs(groups: &[Option<u32>]) -> Vec<Tab> {
    let count = |g: u32| groups.iter().filter(|&&x| x == Some(g)).count();
    let mut seen: Vec<u32> = Vec::new();
    groups
        .iter()
        .map(|&g| match g {
            Some(g) if count(g) >= 2 => {
                if seen.contains(&g) {
                    Tab::Off
                } else {
                    seen.push(g);
                    Tab::Bundle {
                        n: seen.len(),
                        k: count(g),
                    }
                }
            }
            _ => Tab::Own,
        })
        .collect()
}

/// タスクバーのボタンを出す (`AddTab`) / 消す (`DeleteTab`)。
fn set_tab(hwnd: HWND, on: bool) {
    TASKBAR.with(|t| {
        let mut t = t.borrow_mut();
        if t.is_none() {
            let list: windows::core::Result<ITaskbarList> =
                unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER) };
            match list.and_then(|l| unsafe { l.HrInit() }.map(|_| l)) {
                Ok(l) => *t = Some(l),
                Err(e) => {
                    eprintln!("group: taskbar list failed: {e}");
                    return;
                }
            }
        }
        let Some(l) = t.as_ref() else { return };
        let r = unsafe {
            if on {
                l.AddTab(hwnd)
            } else {
                l.DeleteTab(hwnd)
            }
        };
        if let Err(e) = r {
            eprintln!("group: taskbar tab {on} failed: {e}");
        }
    });
}

fn member(hwnd: HWND) -> Option<Member> {
    MEMBERS.with(|m| m.borrow().iter().find(|x| x.hwnd == hwnd).copied())
}

/// そのピンの束の id。
pub fn group_of(hwnd: HWND) -> Option<u32> {
    member(hwnd).and_then(|x| x.group)
}

fn selected_count() -> usize {
    MEMBERS.with(|m| m.borrow().iter().filter(|x| x.selected).count())
}

/// 束 `g` のピン (開いた順)。
fn group_hwnds(g: u32) -> Vec<HWND> {
    MEMBERS.with(|m| {
        m.borrow()
            .iter()
            .filter(|x| x.group == Some(g))
            .map(|x| x.hwnd)
            .collect()
    })
}

/// 一緒に操作を受けるピン (自分も入る、開いた順): 2 枚以上選んだピンなら選んだピン全部 (簡易の束)、
/// 束のピンならその束。どちらでもなければ空。選択を束より先に見る (`lead` と同じ)。
pub fn pack(hwnd: HWND) -> Vec<HWND> {
    let Some(x) = member(hwnd) else {
        return Vec::new();
    };
    if x.selected && selected_count() >= 2 {
        return MEMBERS.with(|m| {
            m.borrow()
                .iter()
                .filter(|y| y.selected)
                .map(|y| y.hwnd)
                .collect()
        });
    }
    x.group.map_or_else(Vec::new, group_hwnds)
}

/// 束 (簡易の束も) に入っている。
pub fn in_pack(hwnd: HWND) -> bool {
    !pack(hwnd).is_empty()
}

/// 同じ束 (簡易の束も) のほかのピン。
fn mates(hwnd: HWND) -> Vec<HWND> {
    pack(hwnd).into_iter().filter(|&h| h != hwnd).collect()
}

/// 束 (簡易の束も) のうち、`hwnd` から Z 順で一番手前の静止画ピン。束のツールバーはこのピンだけが出す。
/// 束でなければ `hwnd`。
/// ponytail: 静止画ピンだけ見る。GIF ピンは自分のツールバーを出したまま。
pub fn leader(hwnd: HWND) -> HWND {
    let pack = pack(hwnd);
    let mut front = hwnd;
    if pack.is_empty() {
        return front;
    }
    let mut h = hwnd;
    while let Ok(prev) = unsafe { GetWindow(h, GW_HWNDPREV) } {
        h = prev;
        if pack.contains(&h) && find(h).is_some() {
            front = h;
        }
    }
    front
}

/// ツールバーの左端の 1 群 (DS `PinGroup`)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lead {
    /// 「束ねる」: 2 枚以上選んでいる間、選んだピンに出す。
    Group,
    /// 束のボタン (束の枚数): 束のピンに出す。押すと束の一覧を開く。
    Bundle(usize),
}

pub fn lead(hwnd: HWND) -> Option<Lead> {
    let x = member(hwnd)?;
    if x.selected && selected_count() >= 2 {
        Some(Lead::Group)
    } else {
        x.group.map(|g| Lead::Bundle(group_hwnds(g).len()))
    }
}

/// ツールバーの並び: 左端の 1 群 (あれば) + `base`。
pub fn lead_items(hwnd: HWND, base: &[Item]) -> Vec<Item> {
    let mut v = Vec::with_capacity(base.len() + 2);
    if lead(hwnd).is_some() {
        v.extend([Item::Tool("group"), Item::Sep]);
    }
    v.extend_from_slice(base);
    v
}

/// 並びの先頭の左端の 1 群の数 (0 か 2)。並びの添字からこれを引くと `ITEMS` の添字。
pub fn lead_len(items: &[Item]) -> usize {
    if items.first() == Some(&Item::Tool("group")) {
        2
    } else {
        0
    }
}

/// 左端のボタンのツールチップ。
pub fn lead_tip(lead: Lead) -> (String, &'static str) {
    let t = crate::strings();
    match lead {
        Lead::Group => (t.group.to_string(), "Ctrl+G"),
        Lead::Bundle(_) => (t.ungroup.to_string(), "Ctrl+G"),
    }
}

/// 束のピンを閉じる (閉じるのボタン・Esc) ときの一覧の行: (文字, キー)。閉じるのボタンの下に出す。
fn group_rows() -> [(&'static str, &'static str); 2] {
    let t = crate::strings();
    [(t.close_one, ""), (t.close_group, "Shift+Esc")]
}

/// 束の一覧の大きさ (`Controls` の一覧。幅は中身の最大で最小 180)。
pub fn group_list_size(s: f32) -> (i32, i32) {
    let w = group_rows()
        .iter()
        .map(|(l, k)| {
            let l = ui::text(l, Style::BodyTabular, s).w;
            let k = ui::text(k, Style::BodyTabular, s).w;
            // 余白 4 + 文字の左 12 + 文字 + 間 24 + キー + 右 12 + 余白 4。
            (px(4.0, s) * 2 + px(12.0, s) * 2 + px(24.0, s)) as f32 + l + k
        })
        .fold(0.0f32, f32::max);
    widgets::list_size(2, w.ceil() as i32, s)
}

fn group_list_hit(w: i32, x: i32, y: i32, s: f32) -> Option<usize> {
    widgets::list_hit(2, w, x, y, s)
}

/// 束の一覧の中身: 行の文字と、行の右にキー (`text-secondary`、右から 12)。
pub fn draw_group_list(
    ctx: &ID2D1DeviceContext,
    hover: Option<usize>,
    w: i32,
    pal: &Palette,
    s: f32,
) {
    let rows = group_rows();
    let labels: Vec<String> = rows.iter().map(|r| r.0.to_string()).collect();
    widgets::draw_list(ctx, &labels, None, hover, w, pal, s);
    for (i, (_, key)) in rows.iter().enumerate() {
        let k = ui::text(key, Style::BodyTabular, s);
        let top = px(4.0, s) + i as i32 * (px(32.0, s) + px(2.0, s));
        ui::draw_text(
            ctx,
            &k,
            (w - px(4.0, s) - px(12.0, s)) as f32 - k.w,
            top as f32 + ((px(32.0, s) as f32 - k.h) / 2.0).round(),
            pal.text_secondary,
        );
    }
}

/// 左端のボタンを押した: 束ねるか束を解くかのトグル。「束ねる」は選んだピンを束ね、束のボタンは束を解く。
pub fn lead_pressed(hwnd: HWND) {
    if lead(hwnd).is_some() {
        toggle_group(hwnd);
    }
}

/// 閉じるときの一覧で選んだ: 0 はこのピンだけ閉じる、1 は束ごと閉じる。
pub fn group_row_chosen(hwnd: HWND, row: usize) {
    match row {
        0 => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        1 => close_group(hwnd),
        _ => {}
    }
}

/// ピンを描き直す (静止画ピンと GIF ピン)。出ているツールバーも並びを直す (左端の 1 群が出入りする)。
fn refresh(hwnd: HWND) {
    if let Some(pin) = find(hwnd) {
        if let Ok(p) = pin.try_borrow() {
            paint(&p);
        }
        if with_hud(|h| h.owner == Some(hwnd) && h.toolbar.visible()).unwrap_or(false) {
            with_hud(close_popup);
            place_hud();
        }
    } else {
        crate::gifpin::refresh(hwnd);
    }
}

fn refresh_all() {
    let all: Vec<HWND> = MEMBERS.with(|m| m.borrow().iter().map(|x| x.hwnd).collect());
    for h in all {
        refresh(h);
    }
}

/// 注釈モードでないピンの Ctrl+クリック: 選ぶ / 外す。束のピンなら束の全員を同じに揃える。
pub fn toggle_select(hwnd: HWND) {
    let on = MEMBERS.with(|m| {
        let mut m = m.borrow_mut();
        let x = m.iter().find(|x| x.hwnd == hwnd)?;
        let (on, g) = (!x.selected, x.group);
        for y in m.iter_mut() {
            if y.hwnd == hwnd || (g.is_some() && y.group == g) {
                y.selected = on;
            }
        }
        Some(on)
    });
    eprintln!("group: select {on:?}, {} selected", selected_count());
    // 2 枚以上で簡易の束になる。束のピンは注釈モードに入れないので、選ぶ前から入っていたピンは出す。
    if selected_count() >= 2 {
        for h in pack(hwnd) {
            leave_edit(h);
        }
    }
    refresh_all();
}

/// 選んでいないピンを (Ctrl なしで) 押した: 選択を全部外す (簡易の束を解く)。
pub fn clear_selection(hwnd: HWND) {
    if member(hwnd).is_none_or(|x| x.selected) || selected_count() == 0 {
        return;
    }
    MEMBERS.with(|m| {
        for x in m.borrow_mut().iter_mut() {
            x.selected = false;
        }
    });
    eprintln!("group: selection cleared");
    refresh_all();
}

/// Ctrl+G・「束ねる」: 選んだピン (2 枚以上) を新しい束にし、選択を外す。
pub fn group_selected() {
    if selected_count() < 2 {
        return;
    }
    let g = NEXT_GROUP.with(|n| n.replace(n.get() + 1));
    let k = MEMBERS.with(|m| {
        let mut m = m.borrow_mut();
        let mut k = 0;
        for x in m.iter_mut().filter(|x| x.selected) {
            x.group = Some(g);
            x.selected = false;
            k += 1;
        }
        k
    });
    eprintln!("group: {g} made of {k}");
    retitle();
    // カーソルが新しい束のピンに乗っていれば、動かさなくても枠を出す。
    hover_left();
    refresh_all();
}

/// Ctrl+G: グループのピンならそのグループを解き、そうでなければ選んだピンを束ねる。
pub fn toggle_group(hwnd: HWND) {
    if group_of(hwnd).is_some() {
        ungroup(hwnd);
    } else {
        group_selected();
    }
}

/// Ctrl+G (グループのピン)・「束を解く」: そのピンの束を全員から外し、全員を選んだ状態にする。
pub fn ungroup(hwnd: HWND) {
    let Some(g) = group_of(hwnd) else { return };
    MEMBERS.with(|m| {
        // 解いたピンは選んだ状態に戻す (Ctrl+G ですぐ束ね直せる)。
        for x in m.borrow_mut().iter_mut().filter(|x| x.group == Some(g)) {
            x.group = None;
            x.selected = true;
        }
    });
    HOVER_GROUP.with(|h| h.set(None));
    eprintln!("group: {g} ungrouped");
    retitle();
    refresh_all();
}

/// Shift+Esc・「束ごと閉じる」: 束のピンを全部閉じる。
pub fn close_group(hwnd: HWND) {
    let all = pack(hwnd);
    if all.is_empty() {
        return;
    }
    eprintln!("group: closing {}", all.len());
    for h in all {
        unsafe {
            let _ = DestroyWindow(h);
        }
    }
}

/// 束のピンのドラッグ: 同じ束のほかのピンを同じ量だけ動かす (Alt+ドラッグでは呼ばない)。
pub fn move_mates(hwnd: HWND, dx: i32, dy: i32) {
    if dx == 0 && dy == 0 {
        return;
    }
    for h in mates(hwnd) {
        if let Some(pin) = find(h) {
            {
                let mut p = pin.borrow_mut();
                let (w, ht) = (p.img.right - p.img.left, p.img.bottom - p.img.top);
                let at = POINT {
                    x: p.img.left + dx,
                    y: p.img.top + dy,
                };
                p.img = ui::rect_at(at, w, ht);
            }
            move_to(&pin);
        } else {
            crate::gifpin::nudge(h, dx, dy);
        }
    }
}

/// 束のピンを最小化した / 戻した: 同じ束のほかのピンも合わせる (前面は取らない)。
pub fn sync_minimized(hwnd: HWND, min: bool) {
    for h in mates(hwnd) {
        if unsafe { IsIconic(h) }.as_bool() != min {
            let cmd = if min {
                SW_SHOWMINNOACTIVE
            } else {
                SW_SHOWNOACTIVATE
            };
            unsafe {
                let _ = ShowWindow(h, cmd);
            }
        }
    }
}

/// カーソルがピンに乗った (`Some`) / 外れた (`None`)。束が替われば、前と今の束のピンを描き直す。
pub fn set_hover(hwnd: Option<HWND>) {
    let g = hwnd.and_then(group_of);
    let old = HOVER_GROUP.with(|h| h.replace(g));
    if old == g {
        return;
    }
    for x in [old, g].into_iter().flatten() {
        for h in group_hwnds(x) {
            if let Some(pin) = find(h) {
                if let Ok(p) = pin.try_borrow() {
                    paint(&p);
                }
            } else {
                crate::gifpin::repaint(h);
            }
        }
    }
}

/// ピンの窓かそのツールバーから外れた (`WM_MOUSELEAVE`): カーソルの下の窓が同じ束のピンか、
/// ピンのツールバー・ポップアップなら枠を残す。
pub fn hover_left() {
    let pt = cursor();
    let under = unsafe { WindowFromPoint(pt) };
    // ツールバーを出しているピンの、ツールバーから画像までの隙間 (下は奥のピンかデスクトップ)。
    let gap = with_hud(|h| {
        h.owner.filter(|&o| {
            h.toolbar.visible()
                && find(o)
                    .and_then(|p| {
                        p.try_borrow()
                            .ok()
                            .map(|p| in_toolbar_zone(h.tb, p.img, p.s, pt))
                    })
                    .unwrap_or(false)
        })
    })
    .flatten();
    let hud = || {
        with_hud(|h| {
            h.owner
                .filter(|_| [h.toolbar.hwnd, h.popup.hwnd].contains(&under))
        })
        .flatten()
    };
    let owner = gap
        .or_else(|| member(under).map(|x| x.hwnd))
        .or_else(hud)
        .or_else(|| crate::gifpin::hud_owner(under));
    set_hover(owner);
}

/// 選んだピンのハイライトと印、またはホバー中の束の枠を描く。`image` は窓の中の画像の矩形。
/// クリックを下の窓へ通している間は、印をその印に譲る。
pub fn draw_marks(
    ctx: &ID2D1DeviceContext,
    hwnd: HWND,
    image: RECT,
    clickthrough: bool,
    pal: &Palette,
    s: f32,
) {
    let Some(x) = member(hwnd) else { return };
    if x.selected {
        without_mates(ctx, hwnd, image, || {
            crate::overlay::highlight(ctx, image, s)
        });
        if !clickthrough {
            let inset = px(8.0, s) as f32;
            let (bx, by) = (image.left as f32 + inset, image.top as f32 + inset);
            badge(ctx, "check", bx, by, pal, s);
        }
        return;
    }
    if x.group.is_none() || HOVER_GROUP.with(|h| h.get()) != x.group {
        return;
    }
    // 束の枠: 画像の外 1〜3 (線の中心は外 2)。`hl-dark` の実線 2 の上に `hl-core` の破線 2 (6 描いて 6 空ける)。
    let w = stroke(2.0, s) as f32;
    let d = px(2.0, s) as f32;
    let r = rectf(
        image.left as f32 - d,
        image.top as f32 - d,
        image.right as f32 + d,
        image.bottom as f32 + d,
    );
    unsafe {
        let dash = px(6.0, s) as f32 / w;
        let Ok(style) = ui::gfx().factory.CreateStrokeStyle(
            &D2D1_STROKE_STYLE_PROPERTIES1 {
                dashStyle: D2D1_DASH_STYLE_CUSTOM,
                ..Default::default()
            },
            Some(&[dash, dash]),
        ) else {
            return;
        };
        let (Ok(dark), Ok(core)) = (
            ctx.CreateSolidColorBrush(&ui::HL_DARK, None),
            ctx.CreateSolidColorBrush(&ui::HL_CORE, None),
        ) else {
            return;
        };
        without_mates(ctx, hwnd, image, || {
            ctx.DrawRectangle(&r, &dark, w, None);
            ctx.DrawRectangle(&r, &core, w, &style);
        });
    }
}

/// 束 (簡易の束も) の全員を描き直す。1 枚だけ動いた・拡縮した後、重なりで抜く枠の所が替わる (`without_mates`)。
pub fn repaint_pack(hwnd: HWND) {
    for h in pack(hwnd) {
        if let Some(pin) = find(h) {
            if let Ok(p) = pin.try_borrow() {
                paint(&p);
            }
        } else {
            crate::gifpin::repaint(h);
        }
    }
}

/// 画面の画像の矩形 (静止画ピンと GIF ピン)。
fn screen_img(hwnd: HWND) -> Option<RECT> {
    match find(hwnd) {
        Some(pin) => pin.try_borrow().ok().map(|p| p.img),
        None => crate::gifpin::image_rect(hwnd),
    }
}

/// `f` (束の枠・選んだピンのハイライト) を、同じ束 (簡易の束も) のほかのピンの画像の所を除いて描く。
/// 重なったピンの枠は、重なった所を描かず、全員の外形だけになる。`image` は窓の中の自分の画像の矩形。
fn without_mates(ctx: &ID2D1DeviceContext, hwnd: HWND, image: RECT, f: impl FnOnce()) {
    let Some(own) = screen_img(hwnd) else {
        return f();
    };
    let (dx, dy) = (image.left - own.left, image.top - own.top);
    let others: Vec<RECT> = mates(hwnd)
        .into_iter()
        .filter_map(screen_img)
        .map(|r| RECT {
            left: r.left + dx,
            top: r.top + dy,
            right: r.right + dx,
            bottom: r.bottom + dy,
        })
        .collect();
    let mask = (!others.is_empty())
        .then(|| exclude_rects(&others))
        .flatten();
    let Some(mask) = mask else {
        return f();
    };
    unsafe {
        ctx.PushLayer(
            &D2D1_LAYER_PARAMETERS1 {
                contentBounds: rectf(f32::MIN, f32::MIN, f32::MAX, f32::MAX),
                geometricMask: std::mem::ManuallyDrop::new(Some(mask)),
                maskAntialiasMode: D2D1_ANTIALIAS_MODE_ALIASED,
                maskTransform: windows_numerics::Matrix3x2::identity(),
                opacity: 1.0,
                opacityBrush: std::mem::ManuallyDrop::new(None),
                layerOptions: D2D1_LAYER_OPTIONS1_NONE,
            },
            None,
        );
        f();
        ctx.PopLayer();
    }
}

/// 大きな矩形から `rects` を抜いた形。
fn exclude_rects(rects: &[RECT]) -> Option<ID2D1Geometry> {
    use windows::core::Interface;
    let factory = &ui::gfx().factory;
    unsafe {
        let big = 1.0e6;
        let mut geo: ID2D1Geometry = factory
            .CreateRectangleGeometry(&rectf(-big, -big, big, big))
            .ok()?
            .cast()
            .ok()?;
        for r in rects {
            let hole = factory.CreateRectangleGeometry(&ui::rf(*r)).ok()?;
            let path = factory.CreatePathGeometry().ok()?;
            let sink = path.Open().ok()?;
            geo.CombineWithGeometry(&hole, D2D1_COMBINE_MODE_EXCLUDE, None, 0.25, &sink)
                .ok()?;
            sink.Close().ok()?;
            geo = path.cast().ok()?;
        }
        Some(geo)
    }
}

/// ツールバーから拡大率を変えるときの基準: 画像の右上。ツールバーは右端を画像に揃えて上に出るので、
/// 拡大率のボタンがカーソルの下から逃げない。
pub fn zoom_anchor(img: RECT) -> POINT {
    POINT {
        x: img.right,
        y: img.top,
    }
}

const TIMER_HUD: usize = 1;
const TIMER_TIP: usize = 2;
/// ピンの窓の見回り: 隠れたツールバーの場所のホバー (`probe_toolbar`)。
const TIMER_PROBE: usize = 4;
/// コピー・保存の帯の刻み (ピンの窓に張る)。帯が出ている間だけ回す。
const TIMER_STATUS: usize = 5;
/// D3D11 の 2D テクスチャの辺の上限 (px)。ピンの窓の面はこれを超えられない。
const MAX_TEXTURE: f32 = 16384.0;
/// ボタンの遷移のコマ。
const TIMER_ANIM: usize = 3;

struct Pin {
    hwnd: HWND,
    surface: Surface,
    image: Rc<Image>,
    bitmap: ID2D1Bitmap1,
    /// 画面上の画像の矩形 (物理 px、拡縮後)。
    img: RECT,
    /// 出したときの位置と倍率。
    home: Home,
    zoom: f32,
    /// 20〜100 (%)。
    opacity: u32,
    clickthrough: bool,
    topmost: bool,
    s: f32,
    /// ドラッグ中: カーソルから画像の左上までのずれ。
    drag: Option<POINT>,
    /// 縁のドラッグ中。
    resize: Option<Resize>,
    doc: Doc,
    /// 編集モードの間だけある。
    edit: Option<Editor>,
    /// 影を描き置いた bitmap (`paint` が大きさと s で作り直す)。
    shadow: RefCell<Option<ui::ShadowBitmap>>,
    /// コピー・保存の帯。
    status: Status,
    /// 注釈のモザイクの升の bitmap。
    mosaics: annot::MosaicCache,
    /// Alt+ドラッグ: 束に入れたまま、このピンだけを動かす。
    solo: bool,
    /// タスクバーから最小化している。
    minimized: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Popup {
    Opacity,
    Zoom,
    /// 注釈の色・線幅・文字の大きさ。
    Ann(Choice),
    /// 束のピンを閉じるときの一覧 (このピンだけ閉じる・束ごと閉じる)。
    Group,
}

struct Hud {
    toolbar: Acrylic,
    tooltip: Acrylic,
    popup: Acrylic,
    owner: Option<HWND>,
    s: f32,
    /// いまの並び (ピンのツールバーか注釈のツールバー)。
    items: Vec<Item>,
    hover: Option<usize>,
    pressed: Option<usize>,
    open: Option<Popup>,
    pop_hover: Option<usize>,
    sliding: bool,
    /// ボタンの遷移中の見た目 (添字, 0 ホバー / 1 押下 / 2 オン)。
    anims: anim::Anim<(usize, u8)>,
    tb: RECT,
    pop: RECT,
    work: RECT,
    /// 最後に描いたとき、ピンは出したときの位置と倍率のまま (戻すボタンが押せない) だった。
    home: bool,
}

thread_local! {
    static PINS: RefCell<Vec<Rc<RefCell<Pin>>>> = const { RefCell::new(Vec::new()) };
    static HUD: RefCell<Option<Hud>> = const { RefCell::new(None) };
}

fn find(hwnd: HWND) -> Option<Rc<RefCell<Pin>>> {
    PINS.with(|p| {
        p.try_borrow()
            .ok()?
            .iter()
            .find(|x| x.try_borrow().is_ok_and(|x| x.hwnd == hwnd))
            .cloned()
    })
}

fn register() {
    widgets::register_class(PIN_CLASS, pin_proc);
    widgets::register_class(HUD_CLASS, hud_proc);
}

/// 画像を画面の `at` (物理 px) に、等倍のピンとして出す。
pub fn create(image: Image, at: RECT) {
    let at = POINT {
        x: at.left,
        y: at.top,
    };
    let _ = create_at(image, at, 1.0, true);
}

/// 画像を拡大率 `zoom` のピンとして、左上を `at` (物理 px) に出す。
/// 窓は初めからその大きさで作る (等倍で作ってから縮めると、ファイルの大きな画像はテクスチャの上限を超える)。
/// 窓・面・ビットマップを作れなければ Err (ピンは出ない。テクスチャの上限 16384 px を超える画像もここ)。
pub fn create_zoomed(image: Image, at: POINT, zoom: f32) -> windows::core::Result<()> {
    create_at(image, at, zoom, false)
}

/// `create_zoomed` の本体。`captured` は画面から撮ったピン (`Home::blends`)。
fn create_at(image: Image, at: POINT, zoom: f32, captured: bool) -> windows::core::Result<()> {
    register();
    // 下限は 0.1。ただし呼び手が作業領域に収める倍率として 0.1 未満を渡したら、それを使う。
    let zoom = zoom.clamp(zoom.min(0.1), 8.0);
    let img = ui::rect_at(
        at,
        (image.w as f32 * zoom).round().max(1.0) as i32,
        (image.h as f32 * zoom).round().max(1.0) as i32,
    );
    let hwnd = create_window(PIN_CLASS, pin_window_rect(img, 1.0), "pin")?;
    let surface = match Surface::new(hwnd, 1, 1) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("pin: surface failed: {e}");
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return Err(e);
        }
    };
    let bitmap = match ui::bitmap_from_bgra(&surface.ctx, image.w, image.h, &image.bgra) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("pin: bitmap failed: {e}");
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return Err(e);
        }
    };
    let pin = Rc::new(RefCell::new(Pin {
        hwnd,
        surface,
        image: Rc::new(image),
        bitmap,
        img,
        home: Home {
            img,
            zoom,
            captured,
        },
        zoom,
        opacity: 100,
        clickthrough: false,
        topmost: true,
        s: ui::scale_of(hwnd),
        drag: None,
        resize: None,
        doc: Doc::default(),
        edit: None,
        shadow: RefCell::new(None),
        status: Status::default(),
        mosaics: Default::default(),
        solo: false,
        minimized: false,
    }));
    PINS.with(|p| p.borrow_mut().push(pin.clone()));
    retitle();
    apply_rect(&pin);
    show_now(hwnd);
    unsafe {
        SetTimer(Some(hwnd), TIMER_PROBE, 100, None);
    }
    crate::force_foreground(hwnd);
    eprintln!(
        "pin: {}x{} at ({}, {}), zoom {:.3}, image {}x{}",
        img.right - img.left,
        img.bottom - img.top,
        img.left,
        img.top,
        zoom,
        pin.borrow().image.w,
        pin.borrow().image.h
    );
    Ok(())
}

/// 窓を画像の矩形と s に合わせて置き、描き直す。面の大きさを変えられなければ false (窓は動かさない)。
fn apply_rect(pin: &Rc<RefCell<Pin>>) -> bool {
    for _ in 0..2 {
        let (hwnd, want) = {
            let mut p = pin.borrow_mut();
            let want = pin_window_rect(p.img, p.s);
            if let Err(e) = p.surface.resize(
                (want.right - want.left) as u32,
                (want.bottom - want.top) as u32,
            ) {
                eprintln!("pin: resize failed: {e}");
                return false;
            }
            (p.hwnd, want)
        };
        // 描いてから窓を動かし、すぐ出す (動かしてから描くと、描き終えるまで古い中身が新しい位置に出る)。
        {
            let p = pin.borrow();
            p.surface.hold();
            paint(&p);
            p.surface.align();
        }
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                want.left,
                want.top,
                want.right - want.left,
                want.bottom - want.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        pin.borrow().surface.present();
        // 動かした先で WM_DPICHANGED が来て s が変わったら、もう一度合わせる。
        let again = {
            let p = pin.borrow();
            let mut now = RECT::default();
            unsafe {
                let _ = GetWindowRect(hwnd, &mut now);
            }
            now != pin_window_rect(p.img, p.s)
        };
        if !again {
            break;
        }
    }
    true
}

/// 移動のドラッグ: 大きさも s も変わらないので、描かずに窓だけを動かす (中身は窓に付いて動く)。
/// 動いた先で DPI が変われば、WM_DPICHANGED (`set_scale`) が `apply_rect` で描き直す。
fn move_to(pin: &Rc<RefCell<Pin>>) {
    let (hwnd, want) = {
        let p = pin.borrow();
        (p.hwnd, pin_window_rect(p.img, p.s))
    };
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            want.left,
            want.top,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// s を `dpi` / 96 にする。推奨矩形は使わない: 画像は物理 px のまま、余白と枠だけをこのモニタの s にする。
/// 同じ s なら何もしない (拡大率の変更の知らせの後に WM_DPICHANGED が来ても二重に動かない)。
fn set_scale(pin: &Rc<RefCell<Pin>>, dpi: u32) {
    let s = dpi as f32 / 96.0;
    let old = std::mem::replace(&mut pin.borrow_mut().s, s);
    if old == s {
        return;
    }
    eprintln!("pin: dpi {} -> {dpi}", (old * 96.0).round());
    apply_rect(pin);
    place_hud();
}

/// 拡大率がその場で変わった。モニタの DPI を読み直す (ツール窓だったころは WM_DPICHANGED が来なかった。
/// 今は来るが、`set_scale` は同じ s なら何もしないので二重に動かない)。最小化中のピンは戻すときに読み直す。
pub fn dpi_changed() {
    let pins: Vec<_> = PINS.with(|p| p.borrow().clone());
    for pin in pins {
        let (hwnd, minimized) = {
            let p = pin.borrow();
            (p.hwnd, p.minimized)
        };
        if !minimized {
            set_scale(&pin, crate::monitor_dpi(hwnd));
        }
    }
}

/// タスクバーから最小化した / 戻した (`WM_SIZE`)。最小化で HUD を消す。戻すときは DPI を読み直して置き直す
/// (場所・倍率・不透明度・クリックを下の窓へ通すかはピンが持ったまま)。`apply_rect` の `SetWindowPos` も `WM_SIZE` を
/// 出すので、状態が変わったときだけ動く。
fn set_minimized(pin: &Rc<RefCell<Pin>>, min: bool) {
    if std::mem::replace(&mut pin.borrow_mut().minimized, min) == min {
        return;
    }
    let hwnd = pin.borrow().hwnd;
    if min {
        drop_hud(hwnd);
    } else {
        set_scale(pin, crate::monitor_dpi(hwnd));
        apply_rect(pin);
    }
    eprintln!("pin: minimized {min}");
    sync_minimized(hwnd, min);
}

fn paint(p: &Pin) {
    crate::insight_scope!("pin_paint");
    let pal = ui::palette();
    let s = p.s;
    let local = local_img(p.img, s);
    let a = p.opacity as f32 / 100.0;
    let image = rectf(
        local.left as f32,
        local.top as f32,
        local.right as f32,
        local.bottom as f32,
    );
    p.surface.draw(|ctx| unsafe {
        // shadow-pin 0 12 34 .42: 標準偏差 17、下へ 12。画像の下には落とさない。
        ui::cached_shadow(
            ctx,
            &p.shadow,
            image,
            17.0 * s,
            ui::rgba(0, 0, 0, 0.42),
            (0.0, px(12.0, s) as f32),
            a,
        );
        let fw = stroke(1.0, s) as f32;
        let mut frame = ui::PIN_FRAME;
        frame.a *= a;
        ui::stroke_rect(
            ctx,
            rectf(
                image.left - fw / 2.0,
                image.top - fw / 2.0,
                image.right + fw / 2.0,
                image.bottom + fw / 2.0,
            ),
            fw,
            frame,
        );
        // 拡大はニアレスト、縮小は線形。
        let interp = if p.zoom >= 1.0 {
            D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR
        } else {
            D2D1_INTERPOLATION_MODE_LINEAR
        };
        ctx.DrawBitmap(&p.bitmap, Some(&image), a, interp, None, None);
        annot_edit::draw(ctx, p, local, p.opacity as f32 / 100.0);
        let inset = px(8.0, s) as f32;
        if p.clickthrough {
            badge(
                ctx,
                "clickthrough",
                image.left + inset,
                image.top + inset,
                pal,
                s,
            );
        }
        if (p.zoom - 1.0).abs() > 1e-3 {
            let label = Label::new(
                vec![Part::Text(
                    ui::text(
                        &format!("{}%", (p.zoom * 100.0).round()),
                        Style::NumStrong,
                        s,
                    ),
                    pal.text_primary,
                )],
                s,
            );
            label.draw(
                ctx,
                image.right - inset - label.w as f32,
                image.bottom - inset - label.h as f32,
                pal,
                s,
            );
        }
        draw_marks(ctx, p.hwnd, local, p.clickthrough, pal, s);
        p.status.draw(ctx, image, s);
    });
}

/// クリックを下の窓へ通している間の印の矩形 (`img` と同じ座標系): 画像の左上から 8 内側、32 × 24。
fn badge_rect(img: RECT, s: f32) -> RECT {
    let (inset, w, h) = (px(8.0, s), px(32.0, s), px(24.0, s));
    let at = POINT {
        x: img.left + inset,
        y: img.top + inset,
    };
    ui::rect_at(at, w, h)
}

/// クリックを下の窓へ通している間の印: 32 × 24、clickthrough アイコン 18、`surface-veil`、角丸 4。
/// 透過中もこの印だけはクリックを受け、押すと透過を解除する。
fn badge(ctx: &ID2D1DeviceContext, icon_id: &'static str, x: f32, y: f32, pal: &Palette, s: f32) {
    let (w, h) = (px(32.0, s) as f32, px(24.0, s) as f32);
    ui::fill_round(
        ctx,
        rectf(x, y, x + w, y + h),
        px(4.0, s) as f32,
        pal.surface_veil,
    );
    let icon = 18.0 * s;
    widgets::draw_icon(
        ctx,
        icon_id,
        x + (w - icon) / 2.0,
        y + (h - icon) / 2.0,
        icon,
        pal.text_primary,
    );
}

// ---- 操作 ----

/// コピーと保存の画像: 元の画像 + 全注釈。注釈が無ければ元の画像。
/// 束 (簡易の束も) なら、全員の画像 (GIF ピンは今のフレーム) を画面の並びのまま合成する (`compose`)。
/// 合成した (空きが透明な) 絵なら 2 つ目が真。
fn output(hwnd: HWND) -> Option<(Image, bool)> {
    let mut pack = pack(hwnd);
    if pack.is_empty() {
        pack.push(hwnd);
    }
    // 奥から描く。
    pack.sort_by_key(|&h| std::cmp::Reverse(depth(h)));
    let mut parts = Vec::with_capacity(pack.len());
    for h in pack {
        if let Some(pin) = find(h) {
            finish_text(h, true);
            let p = pin.borrow();
            parts.push((p.img, annot::composite(&p.image, &p.doc.items)));
        } else if let Some(part) = crate::gifpin::snapshot(h) {
            parts.push(part);
        }
    }
    if parts.len() == 1 {
        return parts.pop().map(|p| (p.1, false));
    }
    (!parts.is_empty()).then(|| (compose(&parts), true))
}

fn copy(hwnd: HWND) {
    crate::insight_scope!("pin_copy");
    let Some((image, clear)) = output(hwnd) else {
        return;
    };
    let t = crate::strings();
    let r = if clear {
        crate::export::copy_clear(hwnd, &image)
    } else {
        crate::export::copy(hwnd, &image)
    };
    if let Err(e) = r {
        eprintln!("pin: copy failed: {e}");
        show_status(
            hwnd,
            Band::result(Kind::Warn, t.band_copy_failed, None, None),
        );
    } else {
        eprintln!("pin: copied {}x{}", image.w, image.h);
        show_status(hwnd, Band::result(Kind::Done, t.band_copied, None, None));
    }
}

/// コピー・保存の帯の中身を替えて刻みを回す (静止画は一瞬で終わるので、完了か失敗だけ)。
fn show_status(hwnd: HWND, band: Band) {
    let Some(pin) = find(hwnd) else { return };
    eprintln!(
        "pin: status {:?} {} {:?} {:?}",
        band.kind, band.label, band.size, band.sub
    );
    let mut p = pin.borrow_mut();
    p.status.show(band);
    p.status.tick();
    paint(&p);
    unsafe {
        SetTimer(Some(hwnd), TIMER_STATUS, 16, None);
    }
}

fn status_tick(hwnd: HWND) {
    let Some(pin) = find(hwnd) else { return };
    let mut p = pin.borrow_mut();
    if p.status.tick() {
        paint(&p);
    }
    if !p.status.active() {
        unsafe {
            let _ = KillTimer(Some(hwnd), TIMER_STATUS);
        }
    }
}

fn save(hwnd: HWND) {
    let Some((image, _)) = output(hwnd) else {
        return;
    };
    let editing = is_editing(hwnd);
    // 隠すのは自分の HUD だけ (別のピンの物はそのまま)。
    if with_hud(|h| h.owner == Some(hwnd)).unwrap_or(false) {
        hide_hud();
    }
    let dir = crate::settings().save_dir.clone();
    let t = crate::strings();
    match crate::export::save(hwnd, &image, dir.as_deref()) {
        Ok(Some(path)) => {
            eprintln!("pin: saved {}", path.display());
            show_status(hwnd, Band::result(Kind::Done, t.band_saved, None, None));
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("pin: save failed: {e}");
            show_status(
                hwnd,
                Band::result(Kind::Warn, t.band_save_failed, None, None),
            );
        }
    }
    if editing && is_editing(hwnd) {
        // ダイアログの間に別のピンが注釈モードに入っていたら終える (注釈モードは 1 つずつ)。
        // そうしないと `show_hud` が拒み、このピンは注釈モードのままツールバーを失う。
        if let Some(other) = with_hud(|h| h.owner.filter(|&o| o != hwnd)).flatten() {
            leave_edit(other);
        }
        show_hud(hwnd);
    }
}

/// 閉じるのボタン・Esc: 束のピンなら、閉じるのボタンの下に「このピンだけ閉じる / 束ごと閉じる」の一覧を出す
/// (外をクリックするか Esc でやめる)。束でなければ閉じる。ツールバーを出せない (クリックを下の窓へ通している
/// など) ときも閉じる。
/// 束の奥のピン (ツールバーを出さない) の Esc は、一番手前のピンのツールバーに一覧を出す
/// (「このピンだけ閉じる」は Esc を受けたピンを閉じる)。
fn close(hwnd: HWND) {
    if in_pack(hwnd) {
        let owner = leader(hwnd);
        show_hud(owner);
        if with_hud(|h| h.owner == Some(owner) && h.toolbar.visible()).unwrap_or(false) {
            CLOSE_TARGET.with(|c| c.set(Some(hwnd)));
            // もう一度押すと一覧を閉じる (やめる)。
            let was = with_hud(|h| {
                let was = h.open == Some(Popup::Group);
                if h.open.is_some() {
                    close_popup(h);
                }
                was
            })
            .unwrap_or(false);
            if !was {
                toggle_popup(Popup::Group);
            }
            paint_hud();
            return;
        }
    }
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// 出したときの位置と倍率 (`Home`) へ戻す。グループのほかのピンは動かさない (拡大率と同じく 1 枚ずつ)。
/// ツールバーは先にその場で消し、戻した先にカーソルがあるときだけ出し直す
/// (置き直すと、カーソルの無い新しい場所に 1 コマ出てから消える)。
fn restore(hwnd: HWND) {
    finish_text(hwnd, true);
    let Some(pin) = find(hwnd) else { return };
    with_hud(|h| h.toolbar.hide_now());
    hide_hud();
    let before = {
        let mut p = pin.borrow_mut();
        let before = (p.zoom, p.img);
        (p.zoom, p.img) = (p.home.zoom, p.home.img);
        before
    };
    if !apply_rect(&pin) {
        let mut p = pin.borrow_mut();
        (p.zoom, p.img) = before;
        return;
    }
    let over = {
        let p = pin.borrow();
        let pt = cursor();
        contains(&pin_window_rect(p.img, p.s), pt)
            || contains(
                &toolbar_rect(p.img, p.s, &lead_items(hwnd, &ITEMS), &front_images(hwnd)),
                pt,
            )
    };
    if over {
        show_hud(hwnd);
    }
    eprintln!("pin: restored to home");
}

fn set_zoom(hwnd: HWND, zoom: f32, around: POINT, even: bool) {
    finish_text(hwnd, true);
    let Some(pin) = find(hwnd) else { return };
    let before = {
        let p = pin.borrow();
        (p.zoom, p.img)
    };
    {
        let mut p = pin.borrow_mut();
        let (zoom, img) = zoom_rect(
            (p.image.w, p.image.h),
            p.zoom,
            p.img,
            zoom,
            around,
            p.s,
            if even {
                1.0
            } else {
                aspect(p.image.h, p.zoom, p.img)
            },
        );
        p.zoom = zoom;
        p.img = rescue_rect(img, work_area(img));
    }
    if !apply_rect(&pin) {
        let mut p = pin.borrow_mut();
        (p.zoom, p.img) = before;
        return;
    }
    // ピンのドラッグ中: 押した点からのずれを倍率の比で掛け直す (そのままだと次の移動で画像がカーソルから跳ぶ)。
    {
        let mut p = pin.borrow_mut();
        let k = p.zoom / before.0;
        if let Some(d) = p.drag.as_mut() {
            d.x = (d.x as f32 * k).round() as i32;
            d.y = (d.y as f32 * k).round() as i32;
        }
    }
    place_hud();
    let p = pin.borrow();
    eprintln!(
        "pin: zoom {:.3}, image {}x{}",
        p.zoom,
        p.img.right - p.img.left,
        p.img.bottom - p.img.top
    );
}

/// ホイール 1 ノッチの不透明度: 10 ずつ、20〜100。
fn opacity_step(v: u32, delta: i16) -> u32 {
    (v as i32 + if delta > 0 { 10 } else { -10 }).clamp(20, 100) as u32
}

/// 縁のドラッグの途中: 押したときの矩形と倍率から置き直す (毎回押したときから求めるので、丸めがたまらない)。
fn resize_to(pin: &Rc<RefCell<Pin>>, r: &Resize) {
    let before = {
        let mut p = pin.borrow_mut();
        let (zoom, img) = r.at((p.image.w, p.image.h), cursor(), p.s, shift());
        if img == p.img {
            return;
        }
        let before = (p.zoom, p.img);
        (p.zoom, p.img) = (zoom, img);
        before
    };
    if !apply_rect(pin) {
        let mut p = pin.borrow_mut();
        (p.zoom, p.img) = before;
        return;
    }
    // 拡大率のボタンの文字も替わるので描き直す。
    place_hud();
    if let Ok(p) = pin.try_borrow() {
        repaint_pack(p.hwnd);
    }
}

/// 不透明度を変える。束のピンなら、同じ束の静止画ピン全部に同じ値を付ける (GIF ピンには不透明度が無い)。
/// 画像の上のホイールの拡縮: グループのピンは一緒に、カーソルを中心に同じ比で拡縮する (並びも同じ比で広がる)。
/// ツールバーの拡大率は 1 枚ずつ (`set_zoom`)。静止画ピンと GIF ピンの両方から呼ぶ。
pub fn wheel_group(hwnd: HWND, delta: i16) {
    let at = cursor();
    for h in std::iter::once(hwnd).chain(mates(hwnd)) {
        if let Some(pin) = find(h) {
            let z = pin.borrow().zoom;
            set_zoom(h, wheel_zoom(z, delta), at, false);
        } else {
            crate::gifpin::wheel_zoom_at(h, delta, at);
        }
    }
    repaint_pack(hwnd);
}

/// ツールバーの拡大率 (一覧・ボタンの上のホイール): `hwnd` を `zoom` にする。束 (簡易の束も) なら、ほかのピンも
/// 同じ比で `around` を中心に拡縮する (並びも同じ比で広がる)。
fn zoom_pack(hwnd: HWND, zoom: f32, around: POINT) {
    let Some(z) = find(hwnd).map(|p| p.borrow().zoom) else {
        return;
    };
    set_zoom(hwnd, zoom, around, false);
    let k = zoom / z;
    for h in mates(hwnd) {
        if let Some(pin) = find(h) {
            let z = pin.borrow().zoom;
            set_zoom(h, z * k, around, false);
        } else {
            crate::gifpin::zoom_by(h, k, around);
        }
    }
    // 持ち主のツールバーはほかのピンの拡縮の後に置き直す (`set_zoom` は今の持ち主の HUD を置く)。
    place_hud();
    repaint_pack(hwnd);
}

fn set_opacity(hwnd: HWND, value: u32) {
    let v = value.clamp(20, 100);
    for h in std::iter::once(hwnd).chain(mates(hwnd)) {
        let Some(pin) = find(h) else { continue };
        pin.borrow_mut().opacity = v;
        paint(&pin.borrow());
    }
    eprintln!("pin: opacity {v}");
}

/// BGRA の画像を左右反転した新しい画像。
fn flip_image(src: &Image) -> Image {
    let row = src.w as usize * 4;
    let mut bgra = Vec::with_capacity(src.bgra.len());
    for line in src.bgra.chunks_exact(row.max(1)) {
        bgra.extend(line.chunks_exact(4).rev().flatten());
    }
    Image {
        w: src.w,
        h: src.h,
        bgra,
    }
}

/// 左右反転: 画素を入れ替える (コピー・保存・モザイクも反転した絵を使う)。注釈も一緒に反転する。もう一度で戻る。
fn flip(hwnd: HWND) {
    finish_text(hwnd, true);
    let Some(pin) = find(hwnd) else { return };
    let mut p = pin.borrow_mut();
    let image = flip_image(&p.image);
    let bitmap = match ui::bitmap_from_bgra(&p.surface.ctx, image.w, image.h, &image.bgra) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("pin: flip failed: {e}");
            return;
        }
    };
    let w = image.w as f32;
    p.image = Rc::new(image);
    p.bitmap = bitmap;
    p.mosaics = Default::default();
    p.doc.flip(w);
    paint(&p);
    eprintln!("pin: flip, {} items", p.doc.items.len());
}

/// クリックを下の窓へ通す / 戻す。束のピンなら、同じ束の静止画ピン全部に効かせる (GIF ピンには無い)。
fn set_clickthrough(hwnd: HWND, on: bool) {
    for h in std::iter::once(hwnd).chain(mates(hwnd)) {
        if find(h).is_some() {
            set_clickthrough_one(h, on);
        }
    }
}

fn set_clickthrough_one(hwnd: HWND, on: bool) {
    if on {
        leave_edit(hwnd);
    }
    let Some(pin) = find(hwnd) else { return };
    pin.borrow_mut().clickthrough = on;
    // 透過中は印の外を余白と同じに扱う: 透過のビットを付け、見回りが印の上でだけ外す。
    if on {
        widgets::pass_margin(hwnd);
    } else {
        widgets::stop_margin_pass(hwnd);
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let bits = (WS_EX_LAYERED | WS_EX_TRANSPARENT).0 as isize;
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !bits);
        }
    }
    if on {
        hide_hud();
    }
    paint(&pin.borrow());
    eprintln!("pin: pass clicks {on}");
}

thread_local! {
    /// 閉じるときの一覧の「このピンだけ閉じる」で閉じるピン (Esc を受けた束の奥のピン)。
    static CLOSE_TARGET: Cell<Option<HWND>> = const { Cell::new(None) };
    /// こちらから重なりの順を変えている間 (`moving_z`)。`hold_z` は止めない。
    static Z_OK: Cell<bool> = const { Cell::new(false) };
}

/// 最前面の切り替えなど、こちらから重なりの順を変える操作を包む (`hold_z` に止めさせない)。
pub fn moving_z(f: impl FnOnce()) {
    Z_OK.with(|z| z.set(true));
    f();
    Z_OK.with(|z| z.set(false));
}

/// ピンの WM_WINDOWPOSCHANGING: グループのピンと、Ctrl を押している間 (Ctrl+クリックでピンを選ぶ) は、重なりの
/// 順を変えない (押したピンが前に出ない。グループ化しても描く順が変わらない)。タスクバーの復元も同じ。
pub fn hold_z(hwnd: HWND, lparam: LPARAM) {
    if Z_OK.with(Cell::get) || (!in_pack(hwnd) && !ctrl()) {
        return;
    }
    unsafe {
        let wp = lparam.0 as *mut WINDOWPOS;
        if let Some(wp) = wp.as_mut() {
            wp.flags |= SWP_NOZORDER;
        }
    }
}

/// ピンを出したときの位置と倍率。「元の位置・縮尺に戻す」の行き先。
#[derive(Clone, Copy)]
pub struct Home {
    pub img: RECT,
    pub zoom: f32,
    /// 画面から撮った (撮った場所に等倍で出る)。
    pub captured: bool,
}

impl Home {
    /// 出したときの位置と倍率のまま (戻すボタンは押せない)。
    pub fn here(&self, img: RECT, zoom: f32) -> bool {
        self.img == img && self.zoom == zoom
    }

    /// 撮った場所に等倍で残っていて、背景と同じ絵。閉じるとき縮めない。
    pub fn blends(&self, img: RECT, zoom: f32) -> bool {
        self.captured && self.here(img, zoom)
    }
}

/// ピンの WM_WINDOWPOSCHANGING: 窓を隠す (閉じる) ときだけ、DWM の閉じる遷移を `on` で付けるか切る。
pub fn close_transition(hwnd: HWND, lparam: LPARAM, on: bool) {
    let hiding = unsafe { (lparam.0 as *const WINDOWPOS).as_ref() }
        .is_some_and(|wp| wp.flags.contains(SWP_HIDEWINDOW));
    if hiding {
        let off = windows::core::BOOL::from(!on);
        let _ = widgets::set_attr(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED, &off);
    }
}

fn set_topmost(hwnd: HWND, on: bool) {
    let Some(pin) = find(hwnd) else { return };
    pin.borrow_mut().topmost = on;
    moving_z(|| ui::set_topmost(hwnd, on));
    with_hud(|h| {
        if h.owner == Some(hwnd) {
            sync_topmost(h, on);
        }
    });
    paint_hud();
    eprintln!("pin: topmost {on}");
}

// ---- 編集モード (注釈エディタは `annot_edit`。ここはホストの実装と HUD の出し入れ) ----

impl annot_edit::Host for Pin {
    fn window(&self) -> HWND {
        self.hwnd
    }
    fn tag(&self) -> &'static str {
        "pin"
    }
    fn img(&self) -> RECT {
        self.img
    }
    fn scale(&self) -> f32 {
        self.s
    }
    fn image(&self) -> &Image {
        &self.image
    }
    fn doc(&self) -> &Doc {
        &self.doc
    }
    fn mosaics(&self) -> (&annot::MosaicCache, usize) {
        (&self.mosaics, 0)
    }
    fn editor(&self) -> Option<&Editor> {
        self.edit.as_ref()
    }
    fn parts(&mut self) -> (&mut Doc, Option<&mut Editor>) {
        (&mut self.doc, self.edit.as_mut())
    }
    fn repaint(&self) {
        paint(self);
    }
    fn toolbar_changed(&self) {
        // 選択が変わって開いているポップアップの役が無くなった (選択を外した・線幅が文字の大きさになった) なら閉じる。
        if self.edit.is_some() {
            let sh = annot_edit::shown(self);
            with_hud(|h| {
                if let Some(Popup::Ann(c)) = h.open
                    && !annot_edit::popup_valid(c, &sh)
                {
                    close_popup(h);
                }
            });
        }
        paint_hud();
    }
}

/// ピンを注釈エディタのホストとして引く。
fn host(hwnd: HWND) -> Option<HostRef> {
    find(hwnd).map(|p| p as HostRef)
}

fn finish_text(hwnd: HWND, commit: bool) -> bool {
    host(hwnd).is_some_and(|h| annot_edit::finish_text(&h, commit))
}

fn is_editing(hwnd: HWND) -> bool {
    find(hwnd).is_some_and(|p| p.try_borrow().is_ok_and(|p| p.edit.is_some()))
}

fn enter_edit(hwnd: HWND) {
    let Some(pin) = find(hwnd) else { return };
    pin.borrow_mut().edit = Some(Editor::new());
    with_hud(|h| {
        close_popup(h);
        drop_tip(h);
    });
    place_hud();
    unsafe {
        // Ctrl+Z などのキーをピンが受ける。
        let _ = SetForegroundWindow(hwnd);
    }
    eprintln!("pin: annotate on");
}

/// 「完了」: 入力中の文字を確定し、ピンのツールバーに戻す。
fn leave_edit(hwnd: HWND) {
    if !is_editing(hwnd) {
        return;
    }
    finish_text(hwnd, true);
    let Some(pin) = find(hwnd) else { return };
    pin.borrow_mut().edit = None;
    with_hud(|h| {
        close_popup(h);
        drop_tip(h);
    });
    paint(&pin.borrow());
    if with_hud(|h| h.owner == Some(hwnd)).unwrap_or(false) {
        place_hud();
    }
    eprintln!("pin: annotate off, {} items", pin.borrow().doc.items.len());
}

/// ピン `hwnd` (静止画・GIF) を押した: ほかの注釈モードのピンを出る (完了と違い、コピーはしない)。
/// ツールバーは 1 個を共有し編集中のピンが握るので、出ないと押したピンにツールバーが来ない。
pub fn leave_other_edits(hwnd: HWND) {
    let others: Vec<HWND> = PINS.with(|p| {
        p.borrow()
            .iter()
            .filter_map(|p| p.try_borrow().ok().map(|p| p.hwnd))
            .filter(|&h| h != hwnd)
            .collect()
    });
    let mut left = false;
    for h in others.into_iter().filter(|&h| is_editing(h)) {
        leave_edit(h);
        left = true;
    }
    crate::gifpin::leave_edits_except(hwnd);
    if left && find(hwnd).is_some() {
        show_hud(hwnd);
    }
}

/// 利用者が編集を終えたとき (完了ボタン・Esc): 注釈モードを出て、注釈込みの画像をコピーする。
fn done_edit(hwnd: HWND) {
    leave_edit(hwnd);
    copy(hwnd);
}

fn set_tool(hwnd: HWND, tool: Tool) {
    if let Some(h) = host(hwnd) {
        annot_edit::set_tool(&h, tool);
    }
    with_hud(|h| {
        close_popup(h);
        // ホバーは残す (押したボタンの上にまだいる)。チップだけ外す。
        let hover = h.hover;
        drop_tip(h);
        h.hover = hover;
    });
    paint_hud();
    eprintln!("pin: tool {tool:?}");
}

/// 開いている束の一覧 (静止画ピンの HUD か、どれかの GIF ピン) を閉じる。閉じたら true。
/// Esc は一覧の持ち主ではなく前面のピンに届く (ツールバーを押しても、そのピンは前面にならない) ので、
/// どのピンが Esc を受けても、静止画ピンと GIF ピンの両方の一覧を見る。
pub fn close_group_lists() -> bool {
    let still = with_hud(|h| h.open == Some(Popup::Group)).unwrap_or(false);
    if still {
        toggle_popup(Popup::Group);
    }
    let gif = crate::gifpin::close_group_list();
    still || gif
}

/// 開いたポップアップを閉じる。
fn close_popup(h: &mut Hud) {
    h.open = None;
    h.pop_hover = None;
    h.popup.hide();
}

/// ホバーとツールチップを外す (ツールバーの並びが替わると、古いチップが次の WM_MOUSEMOVE まで残る)。
fn drop_tip(h: &mut Hud) {
    h.hover = None;
    h.tooltip.hide();
    unsafe {
        let _ = KillTimer(Some(h.toolbar.hwnd), TIMER_TIP);
    }
}

fn history(hwnd: HWND, redo: bool) {
    if let Some(h) = host(hwnd) {
        annot_edit::history(&h, redo);
    }
}

/// 色・線幅・文字の大きさ・拡大率を選んだ。
fn choose(kind: Popup, idx: usize) {
    let Some(owner) = with_hud(|h| h.owner).flatten() else {
        return;
    };
    let Some(pin) = find(owner) else { return };
    let choice = match kind {
        Popup::Zoom => {
            with_hud(close_popup);
            let img = pin.borrow().img;
            zoom_pack(owner, ZOOMS[idx] as f32 / 100.0, zoom_anchor(img));
            paint_hud();
            return;
        }
        Popup::Ann(c) => c,
        Popup::Opacity => return,
        Popup::Group => {
            with_hud(close_popup);
            let target = CLOSE_TARGET.with(|c| c.take()).unwrap_or(owner);
            group_row_chosen(target, idx);
            paint_hud();
            return;
        }
    };
    annot_edit::choose(&(pin.clone() as HostRef), choice, idx);
    // 注釈のポップアップは選んでも閉じない (見比べて選ぶ)。外をクリックすると閉じる。
    paint(&pin.borrow());
    paint_hud();
    eprintln!("pin: choose {choice:?} {idx}");
}

// ---- トレイから ----

/// トレイのピン一覧: (窓, 幅, 高さ, クリックを下の窓へ通しているか)。
pub fn list() -> Vec<(HWND, u32, u32, bool)> {
    PINS.with(|p| {
        p.borrow()
            .iter()
            .map(|x| {
                let x = x.borrow();
                (x.hwnd, x.image.w, x.image.h, x.clickthrough)
            })
            .collect()
    })
}

pub fn release(hwnd: HWND) {
    set_clickthrough(hwnd, false);
}

/// トレイの「閉じる」: 聞かずに閉じる (トレイで 1 枚を選んで押している)。
pub fn close_pin(hwnd: HWND) {
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// OS のテーマが変わった。ピンのラベルと HUD を描き直す。
pub fn theme_changed() {
    let pins: Vec<_> = PINS.with(|p| p.borrow().clone());
    for p in pins {
        paint(&p.borrow());
    }
    let dark = ui::apps_dark();
    HUD.with(|h| {
        if let Some(h) = h.borrow().as_ref() {
            h.toolbar.set_theme(dark);
            h.tooltip.set_theme(dark);
            h.popup.set_theme(dark);
        }
    });
    paint_hud();
}

// ---- HUD (ツールバー・ツールチップ・ポップアップ) ----

fn with_hud<R>(f: impl FnOnce(&mut Hud) -> R) -> Option<R> {
    HUD.with(|h| {
        let mut h = h.try_borrow_mut().ok()?;
        if h.is_none() {
            register();
            let mut toolbar = Acrylic::new(HUD_CLASS, false, Capture::Setting).ok()?;
            toolbar.soft = true;
            let tooltip = Acrylic::new(HUD_CLASS, true, Capture::Setting).ok()?;
            let popup = Acrylic::new(HUD_CLASS, false, Capture::Setting).ok()?;
            *h = Some(Hud {
                toolbar,
                tooltip,
                popup,
                owner: None,
                s: 1.0,
                items: ITEMS.to_vec(),
                hover: None,
                pressed: None,
                open: None,
                pop_hover: None,
                sliding: false,
                anims: anim::Anim::default(),
                tb: RECT::default(),
                pop: RECT::default(),
                work: RECT::default(),
                home: false,
            });
        }
        Some(f(h.as_mut().unwrap()))
    })
}

/// `hwnd` が HUD の持ち主なら、HUD をフェードを待たずにそのコマで消す (閉じた・最小化した)。
fn drop_hud(hwnd: HWND) {
    if with_hud(|h| h.owner == Some(hwnd)).unwrap_or(false) {
        hide_hud();
        with_hud(|h| {
            for a in [&h.toolbar, &h.tooltip, &h.popup] {
                a.hide_now();
            }
        });
    }
}

/// カーソルが `under` より手前の静止画ピンのツールバーの場所にあれば、そのピン。手前のピンのツールバーの場所は
/// 奥のピンの上でも手前のピンが持つ (ツールバーを出していなくても、奥のピンのツールバーに替わらない)。
/// 手前のピンの画像の上なら、そのピン (呼び手が `leader` で束の先頭に寄せる)。
/// ponytail: 静止画ピンどうしだけ。GIF ピンのツールバーの場所は持たない。
fn toolbar_claim(under: HWND) -> Option<HWND> {
    let pt = cursor();
    // ポップアップを開いている HUD は、ツールバーからポップアップまでを持ち主が持つ (隙間は奥のピンの上)。
    let held = with_hud(|h| {
        h.owner
            .filter(|_| h.open.is_some() && h.toolbar.visible())
            .filter(|_| contains(&ui::union(h.tb, h.pop), pt))
    })
    .flatten();
    if held.is_some() {
        return held;
    }
    // `under` より手前の窓 (Z 順で前) を順に見る。
    let mut h = under;
    loop {
        h = unsafe { GetWindow(h, GW_HWNDPREV) }.ok()?;
        let Some(pin) = find(h) else { continue };
        let Ok(p) = pin.try_borrow() else { continue };
        if p.clickthrough || p.minimized {
            continue;
        }
        // 手前のピンの画像の上。余白から画像へ入った直後は、`pass_margin` の透過が外れるまで (≤ 30 ms)
        // 移動が奥のピンに来る。ここで奥のピンを返すと、ツールバーが奥 → 手前と 1 コマ入れ替わってちらつく。
        if contains(&p.img, pt) {
            return Some(h);
        }
        if p.edit.is_some() || leader(h) != h {
            continue;
        }
        let items = lead_items(h, &ITEMS);
        if in_toolbar_zone(
            toolbar_rect(p.img, p.s, &items, &front_images(h)),
            p.img,
            p.s,
            pt,
        ) {
            return Some(h);
        }
    }
}

/// `pt` がツールバー `tb` か、ツールバーから画像 `img` までの隙間 (横はツールバーと枠の外形の重なり) にあるか。
/// 隙間も持たないと、画像からツールバーへ動く途中で奥のピンのツールバーに替わり、ツールバーが点滅する。
/// 横は枠 (幅 `stroke(1, s)`) の列も入れる: ツールバーの右端は枠の外形に揃うので、画像だけだと枠の列が抜ける。
fn in_toolbar_zone(tb: RECT, img: RECT, s: f32, pt: POINT) -> bool {
    let fw = stroke(1.0, s);
    let gap = RECT {
        left: tb.left.max(img.left - fw),
        right: tb.right.min(img.right + fw),
        top: tb.bottom.min(img.bottom),
        bottom: tb.top.max(img.top),
    };
    contains(&tb, pt) || contains(&gap, pt)
}

fn show_hud(owner: HWND) {
    let Some(pin) = find(owner) else { return };
    if pin.borrow().clickthrough || pin.borrow().minimized {
        return;
    }
    // 保存のダイアログの間 (ピンの窓が無効) は出さない。最前面のツールバーがダイアログの上に被って押せた。
    if !unsafe { windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(owner) }.as_bool() {
        return;
    }
    let (shown, other_editing) = with_hud(|h| {
        let other = h.owner.filter(|&o| o != owner && h.toolbar.visible());
        (
            h.owner == Some(owner) && h.toolbar.visible(),
            other.is_some_and(is_editing),
        )
    })
    .unwrap_or((false, false));
    // 編集中のピンのツールバーは、ほかのピンにホバーしても渡さない。
    if shown || other_editing {
        return;
    }
    let topmost = pin.borrow().topmost;
    with_hud(|h| {
        h.owner = Some(owner);
        sync_topmost(h, topmost);
        h.anims.clear();
        h.hover = None;
        h.pressed = None;
        close_popup(h);
        h.tooltip.hide();
    });
    place_hud();
}

/// HUD の最前面を持ち主のピンに揃える (最前面オフのピンの上に HUD だけ浮かない)。
fn sync_topmost(h: &Hud, on: bool) {
    for a in [&h.toolbar, &h.popup, &h.tooltip] {
        a.set_topmost(on);
    }
}

/// ポップアップの幅と、開いたボタンの添字。
fn popup_geometry(kind: Popup, s: f32) -> (usize, i32, i32) {
    match kind {
        Popup::Opacity => {
            let (w, h) = popup_size(s);
            (I_OPACITY, w, h)
        }
        Popup::Zoom => {
            let (w, h) = widgets::list_size(ZOOMS.len(), 0, s);
            (I_ZOOM, w, h)
        }
        Popup::Ann(c) => annot_edit::popup_geometry(c, s),
        Popup::Group => {
            let (w, h) = group_list_size(s);
            (I_CLOSE, w, h)
        }
    }
}

/// ツールバーをピンの置き方 (上 8 → 下 8 → 内側の右上 8) で置き、ポップアップも付いて動かす。
fn place_hud() {
    place_hud_with(true);
}

/// ピンの移動のドラッグに付いて動く。並び・s・大きさが変わらなければ、ツールバーは描かずに窓だけを動かす
/// (中身は位置に依らない)。
fn follow_hud() {
    place_hud_with(false);
}

/// `repaint` が偽なら、並び・s・大きさが変わったときだけツールバーを描く。
fn place_hud_with(repaint: bool) {
    let Some(owner) = with_hud(|h| h.owner).flatten() else {
        return;
    };
    let Some(pin) = find(owner) else { return };
    let (img, s, editing, home) = {
        let p = pin.borrow();
        (p.img, p.s, p.edit.is_some(), p.home.here(p.img, p.zoom))
    };
    let work = work_area(img);
    let items = if editing {
        annot_edit::items()
    } else {
        lead_items(owner, &ITEMS)
    };
    let off = lead_len(&items);
    let (layout, _) = toolbar_layout(&items, s);
    let full = toolbar_rect(img, s, &items, &front_images(owner));
    let state = with_hud(|hud| {
        // 動かして戻すボタンが押せるようになったら描き直す (ドラッグに付いて動く間も)。
        let changed = hud.items != items || hud.s != s || hud.home != home;
        hud.home = home;
        hud.s = s;
        hud.work = work;
        // 注釈の出入りで幅は一度に変える。窓の左端を毎コマ動かす遷移は、窓の移動と描画が揃わずちらつく。
        if hud.items != items {
            hud.anims.clear();
        }
        hud.items = items;
        hud.tb = full;
        // 描いてから窓を動かし、すぐ出す (注釈の出入りで幅が変わる。先に動かすと 1 コマ古い中身が新しい幅に出る)。
        let (w, h) = (
            (full.right - full.left) as u32,
            (full.bottom - full.top) as u32,
        );
        let resized = (hud.toolbar.surface.w, hud.toolbar.surface.h) != (w, h);
        let _ = hud.toolbar.surface.resize(w, h);
        // 描かないときは止めない (`present` は描いていない裏の面を出してしまう)。
        let paint = repaint || changed || resized;
        if paint {
            hud.toolbar.surface.hold();
        }
        (resized, paint)
    });
    let (resized, paint) = state.unwrap_or((false, true));
    if paint {
        paint_hud();
    }
    let open = with_hud(|hud| {
        // 幅が変わるときは合成の直後まで待ってから、窓の移動と中身を続けて出す。
        // 待たないと、2 つが別々の合成に載り、1 コマ古い中身が新しい幅に出る (279 Hz の録画で実測)。
        if resized {
            unsafe {
                let _ = DwmFlush();
            }
        }
        hud.toolbar.show_at(full);
        if paint {
            hud.toolbar.surface.present();
        }
        if let Some(kind) = hud.open {
            let (anchor, pw, ph) = popup_geometry(kind, s);
            // 束の一覧は左端のボタンに揃える。ほかは `ITEMS` の添字なので、左端の 1 群の分ずらす。
            let anchor = anchor + off;
            hud.pop = place_popup(hud.tb.left + layout[anchor].0, hud.tb, pw, ph, work, s);
            hud.popup.show_at(hud.pop);
        }
        unsafe {
            SetTimer(Some(hud.toolbar.hwnd), TIMER_HUD, 100, None);
        }
        hud.open.is_some()
    });
    // ポップアップは置いた後に描く (ツールバーも描き直す)。閉じていれば、ツールバーは上で描いた絵のまま。
    if open == Some(true) {
        paint_hud();
    }
    if editing {
        let (tb, pop) = with_hud(|h| (h.tb, h.open.map(|_| h.pop))).unwrap_or_default();
        eprintln!(
            "hud: annotate toolbar {}x{} at ({}, {}), popup {:?}",
            tb.right - tb.left,
            tb.bottom - tb.top,
            tb.left,
            tb.top,
            pop.map(|r| (
                r.left - tb.left,
                r.top - tb.bottom,
                r.right - r.left,
                r.bottom - r.top
            ))
        );
    }
}

/// ツールバーの矩形 (ピンの置き方: 上 8 → 下 8 → 内側の右上 8)。`front` は `front_images` の結果。
pub fn toolbar_rect(img: RECT, s: f32, items: &[Item], front: &[RECT]) -> RECT {
    let fw = stroke(1.0, s);
    let frame = ui::inflate(img, fw);
    let (_, w) = toolbar_layout(items, s);
    let h = toolbar_height(s);
    let at = place_toolbar(
        frame,
        img,
        w,
        h,
        px(8.0, s),
        px(8.0, s),
        work_area(img),
        front,
    );
    ui::rect_at(at, w, h)
}

/// `hwnd` より手前 (Z 順) にあるピンの画像。カーソルを取るピンだけ (クリックを通すピン・最小化は除く)。
/// ponytail: 静止画のピンだけ見る。手前の GIF のピンが塞ぐ場合も要るなら gifpin から画像を足す。
pub fn front_images(hwnd: HWND) -> Vec<RECT> {
    let mut out = Vec::new();
    let mut h = hwnd;
    while let Ok(prev) = unsafe { GetWindow(h, GW_HWNDPREV) } {
        h = prev;
        if let Some(pin) = find(h)
            && let Ok(p) = pin.try_borrow()
            && !p.clickthrough
            && !p.minimized
        {
            out.push(p.img);
        }
    }
    out
}

/// 隠れたツールバーの場所にカーソルが来たら出す (ピンの窓の外なので WM_MOUSEMOVE が来ない)。
/// ponytail: 最前面オフのピンでは、ほかのアプリの窓がその場所を覆っていても出る。
/// 束の奥のピン (`leader` でない) のツールバーの場所は当たりを持たない。
fn probe_toolbar(hwnd: HWND) {
    let Some(pin) = find(hwnd) else { return };
    if leader(hwnd) != hwnd {
        return;
    }
    let items = lead_items(hwnd, &ITEMS);
    let hit = pin.try_borrow().is_ok_and(|p| {
        !p.clickthrough
            && p.drag.is_none()
            && p.resize.is_none()
            && contains(
                &toolbar_rect(p.img, p.s, &items, &front_images(hwnd)),
                cursor(),
            )
    });
    let shown = with_hud(|h| h.owner == Some(hwnd) && h.toolbar.visible()).unwrap_or(true);
    // 別のピンやそのツールバーがその場所に出ていれば出さない (重なった 2 つが交互に上へ出て震えた)。
    if hit && !shown && !ui::ours_above(hwnd) {
        show_hud(hwnd);
    }
}

/// ツールバーの当たり。
fn hud_hit(h: &Hud, x: i32, y: i32) -> Option<usize> {
    toolbar_hit(&h.items, h.s, x, y)
}

fn hide_hud() {
    with_hud(|h| {
        h.owner = None;
        close_popup(h);
        h.sliding = false;
        h.hover = None;
        h.toolbar.hide();
        h.tooltip.hide();
        unsafe {
            let _ = KillTimer(Some(h.toolbar.hwnd), TIMER_HUD);
            let _ = KillTimer(Some(h.toolbar.hwnd), TIMER_TIP);
        }
    });
}

fn paint_hud() {
    crate::insight_scope!("pin_paint_hud");
    let pal = ui::palette();
    let strings = crate::strings();
    let Some(owner) = with_hud(|h| h.owner).flatten() else {
        return;
    };
    let Some(pin) = find(owner) else { return };
    let Ok(p) = pin.try_borrow() else { return };
    let (opacity, zoom) = (p.opacity, p.zoom);
    let home = p.home.here(p.img, p.zoom);
    let edit = p.edit.as_ref().map(|e| {
        (
            e.tool,
            p.doc.can_undo(),
            p.doc.can_redo(),
            annot_edit::shown(&*p),
        )
    });
    drop(p);
    let lead = lead(owner);
    with_hud(|h| {
        let s = h.s;
        let off = lead_len(&h.items);
        let state = |i: usize| {
            let hover = h.hover == Some(i);
            // 左端の 1 群: 束のボタンはオンの面、一覧を開いている間はオンの押下の面 (`Open` の面ではない)。
            let pressed = h.pressed == Some(i) && hover;
            let flags = match edit {
                None if i < off => (matches!(lead, Some(Lead::Bundle(_))), false, false),
                None => (
                    false,
                    (i - off == I_RESTORE && home) || solo_only(owner, i - off),
                    (i - off == I_OPACITY && h.open == Some(Popup::Opacity))
                        || (i - off == I_ZOOM && h.open == Some(Popup::Zoom)),
                ),
                Some((tool, undo, redo, ref sh)) => annot_edit::button(
                    i,
                    tool,
                    undo,
                    redo,
                    sh,
                    match h.open {
                        Some(Popup::Ann(c)) => Some(c),
                        _ => None,
                    },
                ),
            };
            BtnState::of(flags, hover, pressed)
        };
        let motion = anim::motion();
        let mut anims = std::mem::take(&mut h.anims);
        let looks: Vec<Look> = (0..h.items.len())
            .map(|i| Look::eased(state(i), i, &mut anims, motion))
            .collect();
        let items = &h.items;
        h.toolbar.paint(pal, |ctx| {
            draw_toolbar(ctx, items, |i| looks[i], pal, s);
            if edit.is_none() {
                let (layout, _) = toolbar_layout(items, s);
                let (zx, _) = layout[I_ZOOM + off];
                let top = px(4.0, s) as f32;
                widgets::label_button(
                    ctx,
                    zx as f32,
                    top,
                    &zoom_label(zoom),
                    looks[I_ZOOM + off],
                    pal,
                    s,
                );
            }
            if let Some((.., sh)) = &edit {
                let (layout, _) = toolbar_layout(items, s);
                annot_edit::draw_slots(
                    ctx,
                    &layout,
                    sh,
                    state(annot_edit::A_COLOR),
                    state(annot_edit::A_WIDTH),
                    state(annot_edit::A_DECO),
                    pal,
                    s,
                );
            }
        });
        // 遷移が残っていればコマを刻む (終われば `TIMER_ANIM` で外す)。
        if anims.busy() {
            unsafe {
                SetTimer(Some(h.toolbar.hwnd), TIMER_ANIM, 15, None);
            }
        }
        h.anims = anims;
        let Some(kind) = h.open else { return };
        let pw = (h.pop.right - h.pop.left) as f32;
        let hover = h.pop_hover;
        let sh = edit.map(|e| e.3);
        h.popup.paint(pal, |ctx| match kind {
            Popup::Opacity => widgets::draw_slider_popup(ctx, strings.opacity, opacity, pal, s),
            Popup::Zoom => {
                let labels: Vec<String> = ZOOMS.iter().map(|v| format!("{v}%")).collect();
                widgets::draw_list(ctx, &labels, zoom_sel(zoom), hover, pw as i32, pal, s);
            }
            Popup::Ann(c) => {
                if let Some(sh) = &sh {
                    annot_edit::draw_popup(ctx, c, sh, hover, pw, pal, s)
                }
            }
            Popup::Group => draw_group_list(ctx, hover, pw as i32, pal, s),
        });
    });
}

/// 束 (簡易の束も) では押せないボタン (`ITEMS` の添字): 注釈・元の位置に戻す。左右反転 (メニュー) も束では押せない。
fn solo_only(owner: HWND, i: usize) -> bool {
    matches!(i, I_ANNOTATE | I_RESTORE) && in_pack(owner)
}

/// ツールチップの (名前, キー)。`i` は並びの添字 (`off` は左端の 1 群の数、`lead` はその中身)。
fn tip_text(
    i: usize,
    editing: Option<Tool>,
    off: usize,
    lead: Option<Lead>,
) -> (String, &'static str) {
    let t = crate::strings();
    if let Some(tool) = editing {
        let (n, k) = annot_edit::tip(i, tool);
        return (n.to_string(), k);
    }
    if i < off {
        return lead
            .filter(|_| i == 0)
            .map_or((String::new(), ""), lead_tip);
    }
    let (n, k) = match i - off {
        I_COPY => (t.copy, "Ctrl+C"),
        I_SAVE => (t.save, "Ctrl+S"),
        I_ANNOTATE => (t.annotate, ""),
        I_OPACITY => (t.opacity, ""),
        I_ZOOM => (t.zoom, ""),
        I_RESTORE => (t.restore, ""),
        I_CLOSE => (t.close, "Esc"),
        _ => ("", ""),
    };
    (n.to_string(), k)
}

/// 注釈モードなら、ツールチップとボタンの役を決める道具 (`annot_edit::slot_tool`)。
fn editing_tool(owner: Option<HWND>) -> Option<Tool> {
    owner.and_then(find).and_then(|p| {
        p.try_borrow()
            .ok()
            .and_then(|p| p.edit.as_ref().map(|_| annot_edit::slot_tool(&*p)))
    })
}

fn show_tip() {
    let pal = ui::palette();
    let owner = with_hud(|h| h.owner).flatten();
    let editing = editing_tool(owner);
    let lead = owner.and_then(lead);
    with_hud(|h| {
        let Some(i) = h.hover else { return };
        // 開いたポップアップの見出しに重ねない。
        if h.pressed.is_some() || h.open.is_some() {
            return;
        }
        let s = h.s;
        let (name, key) = tip_text(i, editing, lead_len(&h.items), lead);
        if name.is_empty() {
            return;
        }
        let tip = Tooltip::new(&name, key, s);
        let (layout, _) = toolbar_layout(&h.items, s);
        let (x, bw) = layout[i];
        let top = h.tb.top + px(4.0, s);
        let target = ui::rect_at(
            POINT {
                x: h.tb.left + x,
                y: top,
            },
            bw,
            px(32.0, s),
        );
        tip.show(&mut h.tooltip, target, h.work, pal, s);
    });
}

fn toggle_popup(kind: Popup) {
    with_hud(|h| {
        h.open = if h.open == Some(kind) {
            None
        } else {
            Some(kind)
        };
        h.pop_hover = None;
        h.tooltip.hide();
        if h.open.is_none() {
            h.popup.hide();
        }
    });
    place_hud();
}

fn toolbar_action(i: usize) {
    let Some(owner) = with_hud(|h| h.owner).flatten() else {
        return;
    };
    if editing_tool(Some(owner)).is_some() {
        use annot_edit::Action;
        let Some(sh) =
            find(owner).and_then(|p| p.try_borrow().ok().map(|p| annot_edit::shown(&*p)))
        else {
            return;
        };
        let action = annot_edit::action(i, &sh);
        if let Some(Action::Tool(t)) = action {
            set_tool(owner, t);
            return;
        }
        finish_text(owner, true);
        match action {
            Some(Action::Popup(c)) => toggle_popup(Popup::Ann(c)),
            Some(Action::Fill) => {
                if let Some(pin) = find(owner) {
                    annot_edit::toggle_fill(&(pin.clone() as HostRef));
                    paint(&pin.borrow());
                    paint_hud();
                    eprintln!("pin: toggle fill");
                }
            }
            Some(Action::Undo) => history(owner, false),
            Some(Action::Redo) => history(owner, true),
            Some(Action::Done) => done_edit(owner),
            Some(Action::Tool(_)) | None => {}
        }
        return;
    }
    let off = with_hud(|h| lead_len(&h.items)).unwrap_or(0);
    if i >= off && solo_only(owner, i - off) {
        return;
    }
    if i < off {
        // 「束ねる」は束ねる。束のボタンは束を解く (トグル)。
        if i == 0 {
            lead_pressed(owner);
        }
        return;
    }
    match i - off {
        I_COPY => copy(owner),
        I_SAVE => save(owner),
        I_ANNOTATE => enter_edit(owner),
        I_OPACITY => toggle_popup(Popup::Opacity),
        I_ZOOM => toggle_popup(Popup::Zoom),
        I_RESTORE => restore(owner),
        I_CLOSE => close(owner),
        _ => {}
    }
}

/// 右クリックのメニューの行 (`None` は区切り)。添字は `menu_action` と揃える。
/// コピー・保存 / 左右反転・クリックを下の窓へ・最前面 / 最小化・閉じる。
fn menu_rows(flip_ok: bool, clickthrough: bool, topmost: bool) -> Vec<Option<traymenu::Row>> {
    let t = crate::strings();
    let row = |name, key, check, enabled| {
        Some(traymenu::Row {
            name,
            key,
            check,
            enabled,
        })
    };
    vec![
        row(t.copy, "Ctrl+C", None, true),
        row(t.save, "Ctrl+S", None, true),
        None,
        row(t.flip, "", None, flip_ok),
        row(t.clickthrough, "", Some(clickthrough), true),
        row(t.topmost, "", Some(topmost), true),
        None,
        row(t.minimize, "", None, true),
        row(t.close, "", None, true),
    ]
}

/// 右クリックのメニューを開く。項目はツールバーのボタンと同じく、束なら一番手前のピン (`leader`) に効かせる。
fn open_menu(hwnd: HWND) {
    let owner = leader(hwnd);
    let Some(pin) = find(owner) else { return };
    // 借りたまま開かない (前面を取ると、このピンの窓へメッセージが戻る)。
    let (clickthrough, topmost) = {
        let p = pin.borrow();
        (p.clickthrough, p.topmost)
    };
    let rows = menu_rows(!in_pack(owner), clickthrough, topmost);
    traymenu::popup(cursor(), rows, hwnd, menu_action);
}

/// メニューの行を実行する (メニューはもう閉じている)。`clicked` は右クリックしたピン。
/// 閉じるだけは Esc と同じく右クリックしたピンを渡す (束なら「このピンだけ閉じる」がそのピンを閉じる)。
fn menu_action(clicked: HWND, i: usize) {
    let hwnd = leader(clicked);
    let Some(pin) = find(hwnd) else { return };
    match i {
        0 => {
            eprintln!("pin: menu copy");
            copy(hwnd);
        }
        1 => {
            eprintln!("pin: menu save");
            save(hwnd);
        }
        3 if !in_pack(hwnd) => {
            eprintln!("pin: menu flip");
            flip(hwnd);
        }
        4 => {
            let on = pin.borrow().clickthrough;
            eprintln!("pin: menu clickthrough {}", !on);
            set_clickthrough(hwnd, !on);
        }
        5 => {
            let on = pin.borrow().topmost;
            eprintln!("pin: menu topmost {}", !on);
            set_topmost(hwnd, !on);
        }
        7 => {
            // タスクバーからの最小化と同じ経路 (`WM_SIZE` → `set_minimized`): HUD を消し、束のほかのピンも合わせる。
            eprintln!("pin: menu minimize");
            unsafe {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
            }
        }
        8 => {
            eprintln!("pin: menu close");
            close(clicked);
        }
        _ => {}
    }
}

/// HUD を残すか: 編集中、またはカーソルがピンの窓 (画像と影の余白)・ツールバー・開いたポップアップにある、
/// またはドラッグ中。画像でなく窓の矩形で見るのは、ツールバーが窓の矩形に重なるか接するから
/// (`layout::tests::toolbar_touches_pin_window`): 画像とツールバーの間の隙間で消えない。
fn hud_tick() {
    let pt = cursor();
    let Some(owner) = with_hud(|h| h.owner).flatten() else {
        return;
    };
    let pin = find(owner);
    // 束のツールバーは一番手前のピンが出すので、束のほかのピンの上でも残す。
    let in_mate = mates(owner)
        .into_iter()
        .filter(|&h| find(h).is_some())
        .any(|h| {
            let mut r = RECT::default();
            unsafe { GetWindowRect(h, &mut r) }.is_ok() && contains(&r, pt)
        });
    let in_pin = in_mate
        || pin.as_ref().is_some_and(|p| {
            p.try_borrow().is_ok_and(|p| {
                p.edit.is_some()
                    || (!p.clickthrough
                        && (contains(&pin_window_rect(p.img, p.s), pt)
                            || p.drag.is_some()
                            || p.resize.is_some()))
            })
        });
    // ポップアップはツールバーとの間の隙間も含める (隙間で消えると、ポップアップへ動かす途中で閉じる)。
    let keep = with_hud(|h| {
        in_pin
            || h.sliding
            || contains(&h.tb, pt)
            || (h.open.is_some() && contains(&ui::union(h.tb, h.pop), pt))
    })
    .unwrap_or(false);
    if !keep {
        hide_hud();
        // 隙間から窓の無い所へ出たときは WM_MOUSELEAVE が来ないので、束の枠もここで決め直す。
        hover_left();
    }
}

fn slide(x_screen: i32) {
    let value = with_hud(|h| widgets::slider_value(x_screen - h.pop.left, h.s));
    let owner = with_hud(|h| h.owner).flatten();
    if let (Some(v), Some(o)) = (value, owner) {
        set_opacity(o, v);
        paint_hud();
    }
}

/// 色・線幅・文字の大きさのポップアップの当たり (外形から)。
fn popup_hit(kind: Popup, x: i32, y: i32, s: f32) -> Option<usize> {
    match kind {
        Popup::Ann(c) => annot_edit::popup_hit(c, x, y, s),
        Popup::Zoom => {
            let (w, _) = widgets::list_size(ZOOMS.len(), 0, s);
            widgets::list_hit(ZOOMS.len(), w, x, y, s)
        }
        Popup::Group => {
            let (w, _) = group_list_size(s);
            group_list_hit(w, x, y, s)
        }
        Popup::Opacity => None,
    }
}

unsafe extern "system" fn hud_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let which = HUD.with(|h| {
        h.try_borrow().ok().and_then(|h| {
            h.as_ref().map(|h| {
                if h.toolbar.hwnd == hwnd {
                    1
                } else if h.popup.hwnd == hwnd {
                    2
                } else {
                    0
                }
            })
        })
    });
    // ツールバー・ポップアップの上は持ち主のピンにホバー中 (ピンとの隙間で外れた束の枠を出し直す)。
    if msg == WM_MOUSEMOVE && matches!(which, Some(1 | 2)) {
        set_hover(with_hud(|h| h.owner).flatten());
    }
    let open = with_hud(|h| h.open).flatten();
    let choice = open.filter(|&k| k != Popup::Opacity);
    match (msg, which) {
        (WM_MOUSEACTIVATE, _) => LRESULT(MA_NOACTIVATE as isize),
        (WM_SETCURSOR, _) => {
            // ツールバーのボタンの上は指 (注釈のツールバーは押せるボタンだけ)。
            let id = with_hud(|h| {
                let pin = h.owner.filter(|_| which == Some(1)).and_then(find)?;
                let p = pin.try_borrow().ok()?;
                let mut c = cursor();
                unsafe {
                    let _ = ScreenToClient(hwnd, &mut c);
                }
                let i = hud_hit(h, c.x, c.y);
                Some(match p.edit {
                    Some(_) => annot_edit::bar_cursor(&*p, i),
                    None if i.is_some() => IDC_HAND,
                    None => IDC_ARROW,
                })
            })
            .flatten()
            .unwrap_or(IDC_ARROW);
            unsafe {
                SetCursor(LoadCursorW(None, id).ok());
            }
            LRESULT(1)
        }
        (WM_TIMER, Some(1)) if wparam.0 == TIMER_ANIM => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_ANIM);
            }
            paint_hud();
            LRESULT(0)
        }
        (WM_TIMER, Some(1)) if wparam.0 == TIMER_HUD => {
            hud_tick();
            LRESULT(0)
        }
        (WM_TIMER, Some(1)) if wparam.0 == TIMER_TIP => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_TIP);
            }
            show_tip();
            LRESULT(0)
        }
        (WM_MOUSEMOVE, Some(1)) => {
            let p = lparam_point(lparam);
            let changed = with_hud(|h| {
                let hit = hud_hit(h, p.x, p.y);
                let changed = hit != h.hover;
                h.hover = hit;
                if changed {
                    h.tooltip.hide();
                    unsafe {
                        SetTimer(Some(hwnd), TIMER_TIP, 500, None);
                    }
                }
                changed
            })
            .unwrap_or(false);
            track_leave(hwnd);
            if changed {
                paint_hud();
            }
            LRESULT(0)
        }
        (WM_MOUSELEAVE, Some(1)) => {
            with_hud(|h| {
                h.hover = None;
                h.tooltip.hide();
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_TIP);
                }
            });
            paint_hud();
            hover_left();
            LRESULT(0)
        }
        (WM_LBUTTONDOWN, Some(1)) => {
            let p = lparam_point(lparam);
            with_hud(|h| {
                h.pressed = hud_hit(h, p.x, p.y);
                h.tooltip.hide();
            });
            unsafe {
                SetCapture(hwnd);
            }
            paint_hud();
            LRESULT(0)
        }
        (WM_LBUTTONUP, Some(1)) => {
            let p = lparam_point(lparam);
            // 先に押下を取る: ReleaseCapture は WM_CAPTURECHANGED を同期で送り、押下を消す。
            let fire = with_hud(|h| {
                let hit = hud_hit(h, p.x, p.y);
                let pressed = h.pressed.take();
                (hit == pressed).then_some(hit).flatten()
            })
            .flatten();
            unsafe {
                let _ = ReleaseCapture();
            }
            paint_hud();
            if let Some(i) = fire {
                toolbar_action(i);
            }
            LRESULT(0)
        }
        // 押したまま捕捉を失った (Alt+Tab など): 押下とスライダーのドラッグを捨てる。
        (WM_CAPTURECHANGED, Some(1 | 2)) => {
            with_hud(|h| {
                h.pressed = None;
                h.sliding = false;
            });
            paint_hud();
            LRESULT(0)
        }
        (WM_MOUSEMOVE, Some(2)) if choice.is_some() => {
            let p = lparam_point(lparam);
            let changed = with_hud(|h| {
                let hit = popup_hit(choice.unwrap(), p.x, p.y, h.s);
                let changed = hit != h.pop_hover;
                h.pop_hover = hit;
                changed
            })
            .unwrap_or(false);
            track_leave(hwnd);
            if changed {
                paint_hud();
            }
            LRESULT(0)
        }
        (WM_MOUSELEAVE, Some(2)) => {
            with_hud(|h| h.pop_hover = None);
            paint_hud();
            hover_left();
            LRESULT(0)
        }
        (WM_LBUTTONDOWN, Some(2)) if choice.is_some() => {
            let p = lparam_point(lparam);
            let kind = choice.unwrap();
            if let Some(i) = with_hud(|h| popup_hit(kind, p.x, p.y, h.s)).flatten() {
                choose(kind, i);
            }
            LRESULT(0)
        }
        (WM_LBUTTONDOWN, Some(2)) => {
            with_hud(|h| h.sliding = true);
            unsafe {
                SetCapture(hwnd);
            }
            slide(cursor().x);
            LRESULT(0)
        }
        (WM_MOUSEMOVE, Some(2)) => {
            if with_hud(|h| h.sliding).unwrap_or(false) {
                slide(cursor().x);
            }
            LRESULT(0)
        }
        (WM_LBUTTONUP, Some(2)) => {
            with_hud(|h| h.sliding = false);
            unsafe {
                let _ = ReleaseCapture();
            }
            LRESULT(0)
        }
        // 拡大率のボタンの上のホイールは、ボタンが逃げないよう画像の右上を基準に拡縮する。
        (WM_MOUSEWHEEL, Some(1)) => {
            let delta = (wparam.0 >> 16) as u16 as i16;
            let owner = with_hud(|h| {
                h.owner
                    .filter(|_| h.hover == Some(I_ZOOM + lead_len(&h.items)))
            })
            .flatten();
            if let Some(owner) = owner.filter(|&o| !is_editing(o))
                && let Some(pin) = find(owner)
            {
                let (z, img) = {
                    let p = pin.borrow();
                    (p.zoom, p.img)
                };
                zoom_pack(owner, wheel_zoom(z, delta), zoom_anchor(img));
            }
            LRESULT(0)
        }
        (WM_MOUSEWHEEL, Some(2)) if open == Some(Popup::Opacity) => {
            let delta = (wparam.0 >> 16) as u16 as i16;
            if let Some(owner) = with_hud(|h| h.owner).flatten()
                && let Some(pin) = find(owner)
            {
                let v = pin.borrow().opacity;
                set_opacity(owner, opacity_step(v, delta));
                paint_hud();
            }
            LRESULT(0)
        }
        (WM_PAINT, _) => {
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

// ---- ピンの窓 ----

/// 編集モードのカーソル。
fn edit_cursor(pin: &Pin) -> PCWSTR {
    annot_edit::cursor_id(pin)
}

unsafe extern "system" fn pin_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        // グループのピンと、Ctrl+クリック (ピンを選ぶ) では重なりの順を変えない (押してもアクティブにはなる)。
        WM_WINDOWPOSCHANGING => {
            crate::pin::hold_z(hwnd, lparam);
            let home = find(hwnd)
                .and_then(|p| p.try_borrow().ok().map(|p| p.home.blends(p.img, p.zoom)))
                .unwrap_or(false);
            close_transition(hwnd, lparam, !home);
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        // 枠なし: クライアント領域を窓全体にする (`show_now` が足す `WS_CAPTION` は閉じる遷移のため)。
        WM_NCCALCSIZE => LRESULT(0),
        // アクティブの切り替えで DWM に枠・影を描き直させない (クリックで画像の上の縁がちらついた)。
        WM_NCACTIVATE => LRESULT(1),
        WM_NCHITTEST => {
            let pt = lparam_point(lparam);
            let hit = find(hwnd).and_then(|p| {
                p.try_borrow().ok().map(|p| {
                    let r = if p.clickthrough {
                        badge_rect(p.img, p.s)
                    } else {
                        p.img
                    };
                    contains(&r, pt)
                })
            });
            match hit {
                // ドラッグとクリックの対象は画像の矩形だけ (クリックを下の窓へ通している間は印だけ)。
                // ほかは下へ通す (別プロセスの窓へも)。
                Some(true) => LRESULT(HTCLIENT as isize),
                Some(false) => {
                    widgets::pass_margin(hwnd);
                    LRESULT(HTTRANSPARENT as isize)
                }
                None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
            }
        }
        WM_SETCURSOR => {
            let id = find(hwnd)
                .and_then(|p| {
                    p.try_borrow().ok().map(|p| {
                        if p.clickthrough {
                            IDC_HAND
                        } else if p.edit.is_some() {
                            edit_cursor(&p)
                        } else {
                            edge_hit(p.img, cursor(), p.s).map_or(IDC_SIZEALL, edge_cursor)
                        }
                    })
                })
                .unwrap_or(IDC_SIZEALL);
            unsafe {
                SetCursor(LoadCursorW(None, id).ok());
            }
            LRESULT(1)
        }
        WM_LBUTTONDOWN => {
            leave_other_edits(hwnd);
            // クリックを下の窓へ通している間、クリックが届くのは印の上だけ。押したらピンに戻す。
            if find(hwnd).is_some_and(|p| p.borrow().clickthrough) {
                set_clickthrough(hwnd, false);
                return LRESULT(0);
            }
            with_hud(close_popup);
            paint_hud();
            if is_editing(hwnd)
                && let Some(h) = host(hwnd)
            {
                annot_edit::down(&h);
                return LRESULT(0);
            }
            // 注釈モードでない Ctrl+クリックはピンを選ぶ / 外す (ドラッグしない)。
            if ctrl() {
                toggle_select(hwnd);
                return LRESULT(0);
            }
            clear_selection(hwnd);
            if let Some(pin) = find(hwnd) {
                let c = cursor();
                // 縁のダブルクリック: 横の倍率のまま、元の縦横比に戻す (縦の中点を止める)。
                let reset = {
                    let p = pin.borrow();
                    edge_hit(p.img, c, p.s).is_some() && edge_double(c)
                };
                if reset {
                    let (z, img) = {
                        let p = pin.borrow();
                        (p.zoom, p.img)
                    };
                    set_zoom(hwnd, z, zoom_center(img), true);
                    return LRESULT(0);
                }
                let mut p = pin.borrow_mut();
                if let Some(edge) = edge_hit(p.img, c, p.s) {
                    p.resize = Some(Resize {
                        edge,
                        start: c,
                        img: p.img,
                        zoom: p.zoom,
                    });
                } else {
                    p.drag = Some(POINT {
                        x: c.x - p.img.left,
                        y: c.y - p.img.top,
                    });
                    p.solo = alt();
                }
            }
            unsafe {
                SetCapture(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let Some(pin) = find(hwnd) else {
                return LRESULT(0);
            };
            if annot_edit::drag(&*pin) {
                paint(&pin.borrow());
                return LRESULT(0);
            }
            let resize = pin.borrow().resize;
            if let Some(r) = resize {
                resize_to(&pin, &r);
                return LRESULT(0);
            }
            let drag = pin.borrow().drag;
            if let Some(off) = drag {
                let c = cursor();
                let (dx, dy, solo) = {
                    let mut p = pin.borrow_mut();
                    let (w, h) = (p.img.right - p.img.left, p.img.bottom - p.img.top);
                    let (l, t) = (c.x - off.x, c.y - off.y);
                    let d = (l - p.img.left, t - p.img.top, p.solo);
                    p.img = ui::rect_at(POINT { x: l, y: t }, w, h);
                    d
                };
                move_to(&pin);
                follow_hud();
                if solo {
                    repaint_pack(hwnd);
                } else {
                    move_mates(hwnd, dx, dy);
                }
            } else {
                // 束 (簡易の束も) のピンは、一番手前のピンのツールバーを出す。
                let owner = leader(toolbar_claim(hwnd).unwrap_or(hwnd));
                show_hud(owner);
                set_hover(Some(owner));
                track_leave(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED => {
            if let Some(h) = host(hwnd) {
                annot_edit::up(&h);
            }
            if let Some(pin) = find(hwnd)
                && let Ok(mut p) = pin.try_borrow_mut()
            {
                p.drag = None;
                if p.resize.take().is_some() {
                    eprintln!(
                        "pin: edge resize done, zoom {:.3}, image {}x{}",
                        p.zoom,
                        p.img.right - p.img.left,
                        p.img.bottom - p.img.top
                    );
                }
            }
            if msg == WM_LBUTTONUP {
                unsafe {
                    let _ = ReleaseCapture();
                }
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            hover_left();
            LRESULT(0)
        }
        // 右クリックはメニューを開く。注釈モードの間はメニューを出さず、前と同じくツールバーを出す。
        WM_RBUTTONUP => {
            if is_editing(hwnd) {
                show_hud(leader(hwnd));
            } else {
                open_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = (wparam.0 >> 16) as u16 as i16;
            if let Some(pin) = find(hwnd) {
                // Shift+ホイールは不透明度 (不透明度のポップアップの上のホイールと同じ刻み)。
                if shift() {
                    let v = pin.borrow().opacity;
                    set_opacity(hwnd, opacity_step(v, delta));
                    paint_hud();
                } else {
                    wheel_group(hwnd, delta);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
            let editing = is_editing(hwnd);
            match (ctrl, wparam.0 as u16) {
                // 注釈モードで選んでいれば注釈のコピー、ほかは画像のコピー。
                (true, 0x43) => {
                    if !(editing && host(hwnd).is_some_and(|h| annot_edit::copy_selected(&h))) {
                        copy(hwnd);
                    }
                }
                (true, 0x56) if editing => {
                    if let Some(h) = host(hwnd) {
                        annot_edit::paste(&h);
                    }
                }
                // Ctrl+X: 選んでいる注釈を覚えて消す。選んでいなければ何もしない。
                (true, 0x58) if editing => {
                    if let Some(h) = host(hwnd).filter(|h| annot_edit::copy_selected(h)) {
                        annot_edit::delete_selected(&h);
                    }
                }
                (true, 0x47) if !editing => toggle_group(hwnd),
                (true, 0x53) => save(hwnd),
                // Ctrl+Z 元に戻す、Ctrl+Y / Ctrl+Shift+Z やり直す。
                (true, 0x5A) if editing => history(hwnd, shift()),
                (true, 0x59) if editing => history(hwnd, true),
                (false, k) if k == VK_DELETE.0 && editing => {
                    if let Some(h) = host(hwnd) {
                        annot_edit::delete_selected(&h);
                    }
                }
                // ホイールと同じくカーソルを中心に戻す (左上を基準にすると、拡大で左へ伸びたピンが画面外に出る)。
                // カーソルが画像の外なら画像の中心で戻す (外の点を中心にすると、ピンが遠くへ飛ぶ)。
                (true, 0x30) | (true, 0x60) => {
                    let img = find(hwnd).map(|p| p.borrow().img);
                    let c = cursor();
                    let around = match img {
                        Some(r) if !contains(&r, c) => POINT {
                            x: (r.left + r.right) / 2,
                            y: (r.top + r.bottom) / 2,
                        },
                        _ => c,
                    };
                    set_zoom(hwnd, 1.0, around, true);
                }
                // Esc の押しっぱなし (lparam の bit 30 = リピート) は何もしない。1 回目で注釈モードを出て、2 回目で閉じてしまう。
                (false, k) if k == VK_ESCAPE.0 && lparam.0 & (1 << 30) != 0 => {}
                // 編集中の Esc は選択を外し、何も選んでいなければ編集を終える (ピンは閉じない)。
                (false, k) if k == VK_ESCAPE.0 && editing => {
                    if !host(hwnd).is_some_and(|h| annot_edit::deselect(&h)) {
                        done_edit(hwnd);
                    }
                }
                // Shift+Esc は束ごと閉じる (束でなければ Esc と同じく 1 枚)。
                (false, k) if k == VK_ESCAPE.0 && shift() && in_pack(hwnd) => close_group(hwnd),
                // 束の一覧を開いている間の Esc は、一覧だけを閉じる (ピンは閉じない)。Shift+Esc は上の腕で束ごと閉じる。
                // 条件の `close_group_lists` が一覧を閉じる。閉じたらこの腕に入らず、下の `_` で終わる。
                (false, k) if k == VK_ESCAPE.0 && !close_group_lists() => close(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // 最小化中は捨てる (戻すときに `set_minimized` が読み直す)。
            if let Some(pin) = find(hwnd)
                && !pin.borrow().minimized
            {
                set_scale(&pin, (wparam.0 & 0xFFFF) as u32);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            if let Some(pin) = find(hwnd) {
                set_minimized(&pin, wparam.0 as u32 == SIZE_MINIMIZED);
            }
            LRESULT(0)
        }
        WM_DWMSENDICONICTHUMBNAIL | WM_DWMSENDICONICLIVEPREVIEWBITMAP => {
            if let Some(pin) = find(hwnd) {
                let p = pin.borrow();
                let win = pin_window_rect(p.img, p.s);
                let image = annot::composite(&p.image, &p.doc.items);
                let size = (win.right - win.left, win.bottom - win.top);
                send_iconic(hwnd, msg, lparam, &image, size, local_img(p.img, p.s));
            }
            LRESULT(0)
        }
        WM_PAINT => {
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_PROBE => {
            // 最小化中は、元の場所のホバーでツールバーを出さない。
            if !unsafe { IsIconic(hwnd) }.as_bool() {
                probe_toolbar(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_STATUS => {
            status_tick(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            // 入力の欄は持ち主と一緒に消える。GDI の物だけ返す。
            if let Some(pin) = find(hwnd)
                && let Some(e) = pin.borrow_mut().edit.as_mut()
            {
                annot_edit::release_text(e);
            }
            PINS.with(|p| p.borrow_mut().retain(|x| x.borrow().hwnd != hwnd));
            retitle();
            drop_hud(hwnd);
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_PROBE);
                let _ = KillTimer(Some(hwnd), TIMER_STATUS);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 束の合成: 外接矩形の大きさ、空きは透明、後 (手前) のピンが上、画面の倍率で拡縮。
    #[test]
    fn compose_keeps_screen_layout() {
        let px1 = |b: u8| Image {
            w: 1,
            h: 1,
            bgra: vec![b, b, b, 255],
        };
        let r = |l, t, rr, b| RECT {
            left: l,
            top: t,
            right: rr,
            bottom: b,
        };
        // 奥: (10,10) に 2×1 へ拡大した 1×1。手前: (11,10) に 1×1。離れて (13,10) に 1×1。
        let out = compose(&[
            (r(10, 10, 12, 11), px1(1)),
            (r(11, 10, 12, 11), px1(2)),
            (r(13, 10, 14, 11), px1(3)),
        ]);
        assert_eq!((out.w, out.h), (4, 1));
        assert_eq!(
            out.bgra,
            [1, 1, 1, 255, 2, 2, 2, 255, 0, 0, 0, 0, 3, 3, 3, 255]
        );
    }

    #[test]
    fn toolbar_zone_holds_the_gap_to_the_image() {
        let p = |x, y| POINT { x, y };
        let img = r(60, 260, 715, 615);
        for (tb, pt, want) in [
            // 上のツールバー: 本体・画像までの隙間・隙間の横 (画像の外)
            (r(285, 212, 715, 250), p(500, 230), true),
            (r(285, 212, 715, 250), p(500, 255), true),
            (r(30, 212, 715, 250), p(40, 255), false),
            // 右端を枠の外形 (img.right + 1) に揃えたツールバー: 枠の列 x = img.right の隙間
            (r(286, 212, 716, 250), p(715, 255), true),
            (r(286, 212, 716, 250), p(716, 255), false),
            // 下のツールバー: 隙間
            (r(285, 625, 715, 663), p(500, 620), true),
            // 画像の中に重なったツールバー: 本体の外は持たない
            (r(285, 270, 715, 308), p(500, 265), false),
        ] {
            assert_eq!(in_toolbar_zone(tb, img, 1.0, pt), want, "{tb:?} {pt:?}");
        }
    }

    fn r(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    /// 縁の当たり: 画像 (100, 100)〜(300, 200) の内側の帯 6 × s。8 方向、中、帯のすぐ内、画像の外。
    #[test]
    fn edge_hit_table() {
        let img = r(100, 100, 300, 200);
        let at = |x, y| POINT { x, y };
        for (s, b) in [(1.0, 6), (1.5, 9)] {
            let rows = [
                (at(100, 100), Some((-1, -1))),
                (at(299, 100), Some((1, -1))),
                (at(100, 199), Some((-1, 1))),
                (at(299, 199), Some((1, 1))),
                (at(100 + b - 1, 150), Some((-1, 0))),
                (at(300 - b, 150), Some((1, 0))),
                (at(200, 100 + b - 1), Some((0, -1))),
                (at(200, 200 - b), Some((0, 1))),
                // 帯のすぐ内側と中は掴まない。
                (at(100 + b, 150), None),
                (at(200, 100 + b), None),
                (at(299 - b, 199 - b), None),
                (at(200, 150), None),
                // 画像の外 (影の余白) も掴まない。
                (at(99, 150), None),
                (at(300, 150), None),
                (at(200, 200), None),
            ];
            for (pt, want) in rows {
                assert_eq!(edge_hit(img, pt, s), want, "s {s} pt {pt:?}");
            }
        }
        // 小さい画像 (幅 12): 帯は辺の 1/4 (3) までで、中が残る。
        assert_eq!(edge_hit(r(0, 0, 12, 100), at(6, 50), 1.0), None);
    }

    /// 縁のドラッグ: 対角 (辺なら対辺の中点) が動かない。辺と Shift の角は縦横比を保ち、角は変化の大きい軸に従う。
    #[test]
    fn edge_resize_keeps_opposite() {
        let img = r(100, 100, 300, 200);
        let at = |x, y| POINT { x, y };
        let drag_as = |edge, by: POINT, keep: bool| {
            let rz = Resize {
                edge,
                start: at(0, 0),
                img,
                zoom: 1.0,
            };
            rz.at((200, 100), by, 1.0, keep)
        };
        // Shift (keep) の角と辺: 縦横比を保つ。
        let drag = |edge, by| drag_as(edge, by, true);
        // 右下の角を右へ 200 (幅 2 倍)、下へ 10: 幅に従って 2 倍、左上が動かない。
        assert_eq!(drag((1, 1), at(200, 10)), (2.0, r(100, 100, 500, 300)));
        // 左上の角を左上へ (高さの変化が大きい): 右下が動かない。
        assert_eq!(drag((-1, -1), at(-10, -50)), (1.5, r(0, 50, 300, 200)));
        // 左の辺を右へ 100 (幅 1/2): 右の辺と縦の中点が動かない。
        assert_eq!(drag((-1, 0), at(100, 999)), (0.5, r(200, 125, 300, 175)));
        // 下の辺を上へ引ききっても下限 (0.1) で止まり、上の辺が動かない。
        let (z, got) = drag((0, 1), at(0, -500));
        assert_eq!((z, got.top), (0.1, 100));
        // Shift なしの角: 縦横を別に伸ばし、対角が動かない。倍率は横の倍率。
        assert_eq!(
            drag_as((1, 1), at(200, 10), false),
            (2.0, r(100, 100, 500, 210))
        );
        assert_eq!(
            drag_as((-1, -1), at(-10, -50), false),
            (1.05, r(90, 50, 300, 200))
        );
        // Shift なしの辺: 引いた向きだけ伸び、対辺と他方の軸は動かない。
        assert_eq!(
            drag_as((-1, 0), at(100, 999), false),
            (0.5, r(200, 100, 300, 200))
        );
        assert_eq!(
            drag_as((0, 1), at(999, 50), false),
            (1.0, r(100, 100, 300, 250))
        );
    }

    /// タスクバーのボタン: 束は最初に開いたピンだけがボタンを持ち、番号は代表の開いた順。1 枚の束はピンに戻る。
    #[test]
    fn tabs_table() {
        use Tab::*;
        let rows: [(&[Option<u32>], &[Tab]); 5] = [
            (&[None, None], &[Own, Own]),
            // ピン 3 枚のうち 2 枚を束ねる: 代表 1 つ、ほかは消す、束ねていないピンはそのまま。
            (
                &[Some(7), None, Some(7)],
                &[Bundle { n: 1, k: 2 }, Own, Off],
            ),
            // 束が 2 つ: 番号は代表の開いた順 (id の大小ではない)。
            (
                &[Some(9), Some(4), Some(9), Some(4), Some(4)],
                &[Bundle { n: 1, k: 2 }, Bundle { n: 2, k: 3 }, Off, Off, Off],
            ),
            // 代表を閉じた: 次に開いたピンが代表になる。
            (
                &[None, Some(3), Some(3)],
                &[Own, Bundle { n: 1, k: 2 }, Off],
            ),
            // 1 枚だけ残った束はピンの題に戻す。
            (&[Some(5), None], &[Own, Own]),
        ];
        for (groups, want) in rows {
            assert_eq!(tabs(groups), want, "{groups:?}");
        }
    }

    /// Shift+ホイール 1 ノッチで 10、20 未満と 100 超にならない。
    #[test]
    fn opacity_step_table() {
        for (v, d, want) in [
            (50, 120, 60),
            (50, -120, 40),
            (100, 120, 100),
            (20, -120, 20),
            (25, -120, 20),
        ] {
            assert_eq!(opacity_step(v, d), want, "{v} {d}");
        }
    }

    /// 拡縮の矩形の契約: テクスチャの上限、1.0 への吸着、`around` を保つ置き直し、画面外の救済。
    #[test]
    fn zoom_rect_table() {
        let at = |x, y| POINT { x, y };
        // (a) 幅 2100 の画像に 8 倍を求めると、窓 (画像 + 余白) が 16384 に収まる倍率で止まる。
        let (z, img) = zoom_rect(
            (2100, 100),
            1.0,
            r(0, 0, 2100, 100),
            8.0,
            at(0, 0),
            1.0,
            1.0,
        );
        let win = pin_window_rect(img, 1.0);
        assert!(z < 8.0 && z > 7.7, "zoom {z}");
        assert!(
            win.right - win.left <= 16384,
            "window {}",
            win.right - win.left
        );
        // (b) 0.99〜1.01 は 1.0 に吸着し、外は吸着しない。
        for (want, got) in [(0.995, 1.0), (1.009, 1.0), (1.02, 1.02), (0.98, 0.98)] {
            let (z, _) = zoom_rect((200, 100), 1.0, r(0, 0, 200, 100), want, at(0, 0), 1.0, 1.0);
            assert_eq!(z, got, "want {want}");
        }
        // (c) `around` (画像の左から 50、上から 20) は 2 倍の後も画像の同じ位置 (100, 40) を指す。
        let (z, img) = zoom_rect(
            (200, 100),
            1.0,
            r(100, 100, 300, 200),
            2.0,
            at(150, 120),
            1.0,
            1.0,
        );
        assert_eq!((z, img), (2.0, r(50, 80, 450, 280)));
        // (e) 下限は min(0.1, 今の倍率)。0.08 倍のピンを縮めても 0.08 のまま、拡大はそのまま効く。1.0 倍からは 0.1 で止まる。
        for (zoom, want, got) in [(0.08, 0.064, 0.08), (0.08, 0.1, 0.1), (1.0, 0.05, 0.1)] {
            let (z, _) = zoom_rect(
                (200, 100),
                zoom,
                r(0, 0, 200, 100),
                want,
                at(0, 0),
                1.0,
                1.0,
            );
            assert_eq!(z, got, "zoom {zoom} want {want}");
        }
        // (f) 隅で縦を 1.1 倍に伸ばしたピン (2 倍、400 × 220) を 1 倍にしても、縦横比はそのまま。
        let (z, img) = zoom_rect((200, 100), 2.0, r(0, 0, 400, 220), 1.0, at(0, 0), 1.0, 1.1);
        assert_eq!((z, img), (1.0, r(0, 0, 200, 110)));
        // (d) 作業領域と交わらない矩形は左上へ寄せ、交わる矩形はそのまま。
        let work = r(0, 0, 1920, 1040);
        let rescue = crate::layout::rescue_rect;
        assert_eq!(rescue(r(-5000, 0, -4000, 100), work), r(0, 0, 1000, 100));
        assert_eq!(rescue(r(-500, 10, 500, 110), work), r(-500, 10, 500, 110));
    }
}
