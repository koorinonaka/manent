//! 静止画ピン: キャプチャした位置に残る枠なしの窓 (DirectComposition の透過窓、物理 px の 1:1)。
//! ホバーで出るツールバー・ツールチップ・ポップアップは別のトップレベル窓 (HUD)。
//! ツールバーの「注釈」で編集モードに入り、注釈 (`annot`) を描く。編集の間はツールバーの中身を置き換える。

use std::cell::RefCell;
use std::rc::Rc;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dwm::{
    DWMWA_TRANSITIONS_FORCEDISABLED, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmFlush,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_DELETE, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use crate::annot::{self, Doc, Tool};
use crate::annot_edit::{self, Choice, Editor, HostRef};
use crate::capture::Image;
use crate::layout::{pin_margins, pin_window_rect, place_toolbar};
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

const ITEMS: [Item; 11] = [
    Item::Tool("copy"),
    Item::Tool("save"),
    Item::Tool("annotate"),
    Item::Sep,
    Item::Tool("opacity"),
    Item::Wide,
    Item::Sep,
    Item::Tool("clickthrough"),
    Item::Tool("topmost"),
    Item::Sep,
    Item::Tool("close"),
];
const I_COPY: usize = 0;
const I_SAVE: usize = 1;
const I_ANNOTATE: usize = 2;
const I_OPACITY: usize = 4;
const I_ZOOM: usize = 5;
const I_CLICKTHROUGH: usize = 7;
const I_TOPMOST: usize = 8;
const I_CLOSE: usize = 10;

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
    zoom: f32,
    /// 20〜100 (%)。
    opacity: u32,
    clickthrough: bool,
    topmost: bool,
    s: f32,
    /// ドラッグ中: カーソルから画像の左上までのずれ。
    drag: Option<POINT>,
    doc: Doc,
    /// 編集モードの間だけある。
    edit: Option<Editor>,
    /// 影を描き置いた bitmap (`paint` が大きさと s で作り直す)。
    shadow: RefCell<Option<ui::ShadowBitmap>>,
    /// コピー・保存の帯。
    status: Status,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Popup {
    Opacity,
    Zoom,
    /// 注釈の色・線幅・文字の大きさ。
    Ann(Choice),
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
}

thread_local! {
    static PINS: RefCell<Vec<Rc<RefCell<Pin>>>> = const { RefCell::new(Vec::new()) };
    static HUD: RefCell<Option<Hud>> = const { RefCell::new(None) };
}

fn find(hwnd: HWND) -> Option<Rc<RefCell<Pin>>> {
    PINS.with(|p| p.borrow().iter().find(|x| x.borrow().hwnd == hwnd).cloned())
}

fn register() {
    thread_local!(static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    if !DONE.with(|d| d.replace(true)) {
        widgets::register_class(PIN_CLASS, pin_proc);
        widgets::register_class(HUD_CLASS, hud_proc);
    }
}

/// 画像を画面の `at` (物理 px) に、等倍のピンとして出す。
pub fn create(image: Image, at: RECT) {
    let _ = create_zoomed(
        image,
        POINT {
            x: at.left,
            y: at.top,
        },
        1.0,
    );
}

/// 画像を拡大率 `zoom` のピンとして、左上を `at` (物理 px) に出す。
/// 窓は初めからその大きさで作る (等倍で作ってから縮めると、ファイルの大きな画像はテクスチャの上限を超える)。
/// 窓・面・ビットマップを作れなければ Err (ピンは出ない。テクスチャの上限 16384 px を超える画像もここ)。
pub fn create_zoomed(image: Image, at: POINT, zoom: f32) -> windows::core::Result<()> {
    register();
    let zoom = zoom.clamp(0.1, 8.0);
    let img = RECT {
        left: at.x,
        top: at.y,
        right: at.x + (image.w as f32 * zoom).round().max(1.0) as i32,
        bottom: at.y + (image.h as f32 * zoom).round().max(1.0) as i32,
    };
    let guess = pin_window_rect(img, 1.0);
    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP,
            PIN_CLASS,
            &HSTRING::from(crate::PRODUCT_NAME),
            WS_POPUP,
            guess.left,
            guess.top,
            guess.right - guess.left,
            guess.bottom - guess.top,
            None,
            None,
            GetModuleHandleW(None).ok().map(Into::into),
            None,
        )
    } {
        Ok(h) => h,
        Err(e) => {
            eprintln!("pin: window failed: {e}");
            return Err(e);
        }
    };
    let _ = widgets::set_attr(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_DONOTROUND);
    let _ = widgets::set_attr(
        hwnd,
        DWMWA_TRANSITIONS_FORCEDISABLED,
        &windows::core::BOOL::from(true),
    );
    ui::exclude_from_capture(hwnd);
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
        zoom,
        opacity: 100,
        clickthrough: false,
        topmost: true,
        s: ui::scale_of(hwnd),
        drag: None,
        doc: Doc::default(),
        edit: None,
        shadow: RefCell::new(None),
        status: Status::default(),
    }));
    PINS.with(|p| p.borrow_mut().push(pin.clone()));
    apply_rect(&pin);
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
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

/// 拡大率がその場で変わった。止まっているピンは WM_DPICHANGED を受けないので、モニタの DPI を読み直す。
pub fn dpi_changed() {
    let pins: Vec<_> = PINS.with(|p| p.borrow().clone());
    for pin in pins {
        let hwnd = pin.borrow().hwnd;
        set_scale(&pin, crate::monitor_dpi(hwnd));
    }
}

/// ピンの窓の中の画像の矩形 (窓の左上から)。
fn local_img(p: &Pin) -> RECT {
    let (ml, mt, _, _) = pin_margins(p.s);
    RECT {
        left: ml,
        top: mt,
        right: ml + p.img.right - p.img.left,
        bottom: mt + p.img.bottom - p.img.top,
    }
}

fn paint(p: &Pin) {
    crate::insight_scope!("pin_paint");
    let pal = ui::palette();
    let s = p.s;
    let local = local_img(p);
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
            badge(ctx, image.left + inset, image.top + inset, pal, s);
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
        p.status.draw(ctx, image, s);
    });
}

/// クリック透過中の印の矩形 (`img` と同じ座標系): 画像の左上から 8 内側、32 × 24。
fn badge_rect(img: RECT, s: f32) -> RECT {
    let (inset, w, h) = (px(8.0, s), px(32.0, s), px(24.0, s));
    RECT {
        left: img.left + inset,
        top: img.top + inset,
        right: img.left + inset + w,
        bottom: img.top + inset + h,
    }
}

/// クリック透過中の印: 32 × 24、clickthrough アイコン 18、`surface-veil`、角丸 4。
/// 透過中もこの印だけはクリックを受け、押すと透過を解除する。
fn badge(ctx: &ID2D1DeviceContext, x: f32, y: f32, pal: &Palette, s: f32) {
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
        "clickthrough",
        x + (w - icon) / 2.0,
        y + (h - icon) / 2.0,
        icon,
        pal.text_primary,
    );
}

// ---- 操作 ----

/// コピーと保存の画像: 元の画像 + 全注釈。注釈が無ければ元の画像。
fn output(hwnd: HWND) -> Option<Image> {
    finish_text(hwnd, true);
    let pin = find(hwnd)?;
    let p = pin.borrow();
    Some(annot::composite(&p.image, &p.doc.items))
}

fn copy(hwnd: HWND) {
    crate::insight_scope!("pin_copy");
    let Some(image) = output(hwnd) else { return };
    let t = crate::strings();
    if let Err(e) = crate::export::copy(hwnd, &image) {
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
    let Some(image) = output(hwnd) else { return };
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

fn close(hwnd: HWND) {
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

fn set_zoom(hwnd: HWND, zoom: f32, around: POINT) {
    finish_text(hwnd, true);
    let Some(pin) = find(hwnd) else { return };
    let before = {
        let p = pin.borrow();
        (p.zoom, p.img)
    };
    {
        let mut p = pin.borrow_mut();
        // 上限は 8 倍と、窓 (画像 + 余白) が D3D のテクスチャ上限に入る倍率の小さい方。
        let (ml, mt, mr, mb) = pin_margins(p.s);
        let max = ((MAX_TEXTURE - (ml + mr) as f32) / p.image.w.max(1) as f32)
            .min((MAX_TEXTURE - (mt + mb) as f32) / p.image.h.max(1) as f32)
            .clamp(0.1, 8.0);
        let zoom = zoom.clamp(0.1, max);
        let zoom = if (zoom - 1.0).abs() < 0.01 { 1.0 } else { zoom };
        let k = zoom / p.zoom;
        let w = (p.image.w as f32 * zoom).round().max(1.0) as i32;
        let h = (p.image.h as f32 * zoom).round().max(1.0) as i32;
        let left = around.x - ((around.x - p.img.left) as f32 * k).round() as i32;
        let top = around.y - ((around.y - p.img.top) as f32 * k).round() as i32;
        p.zoom = zoom;
        p.img = RECT {
            left,
            top,
            right: left + w,
            bottom: top + h,
        };
        // 作業領域と 1 px も交わらなければ (掴めない・閉じられない)、左上を作業領域の左上へ寄せる。
        let work = work_area(p.img);
        let img = p.img;
        if img.right <= work.left
            || img.left >= work.right
            || img.bottom <= work.top
            || img.top >= work.bottom
        {
            p.img = RECT {
                left: work.left,
                top: work.top,
                right: work.left + w,
                bottom: work.top + h,
            };
        }
    }
    if !apply_rect(&pin) {
        let mut p = pin.borrow_mut();
        (p.zoom, p.img) = before;
        return;
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

fn set_opacity(hwnd: HWND, value: u32) {
    let Some(pin) = find(hwnd) else { return };
    pin.borrow_mut().opacity = value.clamp(20, 100);
    paint(&pin.borrow());
    eprintln!("pin: opacity {}", value.clamp(20, 100));
}

fn set_clickthrough(hwnd: HWND, on: bool) {
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
    eprintln!("pin: click-through {on}");
}

fn set_topmost(hwnd: HWND, on: bool) {
    let Some(pin) = find(hwnd) else { return };
    pin.borrow_mut().topmost = on;
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
        h.open = None;
        h.popup.hide();
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
        h.open = None;
        h.popup.hide();
        drop_tip(h);
    });
    paint(&pin.borrow());
    if with_hud(|h| h.owner == Some(hwnd)).unwrap_or(false) {
        place_hud();
    }
    eprintln!("pin: annotate off, {} items", pin.borrow().doc.items.len());
}

fn set_tool(hwnd: HWND, tool: Tool) {
    if let Some(h) = host(hwnd) {
        annot_edit::set_tool(&h, tool);
    }
    with_hud(|h| {
        h.open = None;
        h.popup.hide();
        // ホバーは残す (押したボタンの上にまだいる)。チップだけ外す。
        let hover = h.hover;
        drop_tip(h);
        h.hover = hover;
    });
    paint_hud();
    eprintln!("pin: tool {tool:?}");
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
            with_hud(|h| {
                h.open = None;
                h.pop_hover = None;
                h.popup.hide();
            });
            let img = pin.borrow().img;
            set_zoom(owner, ZOOMS[idx] as f32 / 100.0, zoom_anchor(img));
            paint_hud();
            return;
        }
        Popup::Ann(c) => c,
        Popup::Opacity => return,
    };
    annot_edit::choose(&(pin.clone() as HostRef), choice, idx);
    // 文字の大きさは選んでも閉じない (見比べて選ぶ)。外をクリックすると閉じる。
    if choice != Choice::Size {
        with_hud(|h| {
            h.open = None;
            h.pop_hover = None;
            h.popup.hide();
        });
    }
    paint(&pin.borrow());
    paint_hud();
    eprintln!("pin: choose {choice:?} {idx}");
}

// ---- トレイから ----

/// トレイのピン一覧: (窓, 幅, 高さ, クリック透過中)。
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

pub fn close_pin(hwnd: HWND) {
    close(hwnd);
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
            let toolbar = Acrylic::new(HUD_CLASS, false, Capture::Setting).ok()?;
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
            });
        }
        Some(f(h.as_mut().unwrap()))
    })
}

fn show_hud(owner: HWND) {
    let Some(pin) = find(owner) else { return };
    if pin.borrow().clickthrough {
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
        h.open = None;
        h.popup.hide();
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
    }
}

/// ツールバーをピンの置き方 (上 8 → 下 8 → 内側の右上 8) で置き、ポップアップも付いて動かす。
fn place_hud() {
    let Some(owner) = with_hud(|h| h.owner).flatten() else {
        return;
    };
    let Some(pin) = find(owner) else { return };
    let (img, s, editing) = {
        let p = pin.borrow();
        (p.img, p.s, p.edit.is_some())
    };
    let work = work_area(img);
    let items = if editing {
        annot_edit::items()
    } else {
        ITEMS.to_vec()
    };
    let (layout, _) = toolbar_layout(&items, s);
    let full = toolbar_rect(img, s, &items);
    let resized = with_hud(|hud| {
        hud.s = s;
        hud.work = work;
        // 注釈の出入りで幅は一度に変える。窓の左端を毎コマ動かす遷移は、窓の移動と描画が揃わずちらつく。
        if hud.items != items {
            hud.anims.clear();
        }
        hud.items = items;
        hud.tb = full;
        // 描いてから窓を動かし、すぐ出す (注釈の出入りで幅が変わる。先に動かすと 1 コマ古い中身が新しい幅に出る)。
        let (w, h) = ((full.right - full.left) as u32, (full.bottom - full.top) as u32);
        let resized = (hud.toolbar.surface.w, hud.toolbar.surface.h) != (w, h);
        let _ = hud.toolbar.surface.resize(w, h);
        hud.toolbar.surface.hold();
        resized
    });
    paint_hud();
    with_hud(|hud| {
        // 幅が変わるときは合成の直後まで待ってから、窓の移動と中身を続けて出す。
        // 待たないと、2 つが別々の合成に載り、1 コマ古い中身が新しい幅に出る (279 Hz の録画で実測)。
        if resized == Some(true) {
            unsafe {
                let _ = DwmFlush();
            }
        }
        hud.toolbar.show_at(full);
        hud.toolbar.surface.present();
        if let Some(kind) = hud.open {
            // ポップアップは開いたボタンの下 4、左端を揃える。下に入らなければ上 4。
            let (anchor, pw, ph) = popup_geometry(kind, s);
            let x = hud.tb.left + layout[anchor].0;
            let below = hud.tb.bottom + px(4.0, s);
            let y = if below + ph <= work.bottom {
                below
            } else {
                hud.tb.top - px(4.0, s) - ph
            };
            hud.pop = RECT {
                left: x,
                top: y,
                right: x + pw,
                bottom: y + ph,
            };
            hud.popup.show_at(hud.pop);
        }
        unsafe {
            SetTimer(Some(hud.toolbar.hwnd), TIMER_HUD, 100, None);
        }
    });
    paint_hud();
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

/// ツールバーの矩形 (ピンの置き方: 上 8 → 下 8 → 内側の右上 8)。
fn toolbar_rect(img: RECT, s: f32, items: &[Item]) -> RECT {
    let fw = stroke(1.0, s);
    let frame = RECT {
        left: img.left - fw,
        top: img.top - fw,
        right: img.right + fw,
        bottom: img.bottom + fw,
    };
    let (_, w) = toolbar_layout(items, s);
    let h = toolbar_height(s);
    let at = place_toolbar(frame, img, w, h, px(8.0, s), px(8.0, s), work_area(img));
    RECT {
        left: at.x,
        top: at.y,
        right: at.x + w,
        bottom: at.y + h,
    }
}

/// 隠れたツールバーの場所にカーソルが来たら出す (ピンの窓の外なので WM_MOUSEMOVE が来ない)。
/// ponytail: 最前面オフのピンでは、ほかのアプリの窓がその場所を覆っていても出る。
fn probe_toolbar(hwnd: HWND) {
    let Some(pin) = find(hwnd) else { return };
    let hit = pin.try_borrow().is_ok_and(|p| {
        !p.clickthrough && p.drag.is_none() && contains(&toolbar_rect(p.img, p.s, &ITEMS), cursor())
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
        h.open = None;
        h.sliding = false;
        h.hover = None;
        h.toolbar.hide();
        h.tooltip.hide();
        h.popup.hide();
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
    let (topmost, clickthrough, opacity, zoom) = (p.topmost, p.clickthrough, p.opacity, p.zoom);
    let edit = p
        .edit
        .as_ref()
        .map(|e| (e.tool, p.doc.can_undo(), p.doc.can_redo()));
    drop(p);
    with_hud(|h| {
        let s = h.s;
        let state = |i: usize| {
            let hover = h.hover == Some(i);
            let pressed = h.pressed == Some(i) && hover;
            let (on, disabled, open) = match edit {
                None => (
                    match i {
                        I_TOPMOST => topmost,
                        I_CLICKTHROUGH => clickthrough,
                        _ => false,
                    },
                    false,
                    (i == I_OPACITY && h.open == Some(Popup::Opacity))
                        || (i == I_ZOOM && h.open == Some(Popup::Zoom)),
                ),
                Some((tool, undo, redo)) => annot_edit::button(
                    i,
                    tool,
                    undo,
                    redo,
                    match h.open {
                        Some(Popup::Ann(c)) => Some(c),
                        _ => None,
                    },
                ),
            };
            match (on, disabled, open) {
                (_, true, _) => BtnState::Disabled,
                (_, _, true) => BtnState::Open,
                (true, ..) if pressed => BtnState::OnPressed,
                (true, ..) if hover => BtnState::OnHover,
                (true, ..) => BtnState::On,
                _ if pressed => BtnState::Pressed,
                _ if hover => BtnState::Hover,
                _ => BtnState::Normal,
            }
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
                let (zx, _) = layout[I_ZOOM];
                let top = px(4.0, s) as f32;
                widgets::label_button(
                    ctx,
                    zx as f32,
                    top,
                    &zoom_label(zoom),
                    looks[I_ZOOM],
                    pal,
                    s,
                );
            }
            if let Some((tool, ..)) = edit {
                let (layout, _) = toolbar_layout(items, s);
                annot_edit::draw_slots(
                    ctx,
                    &layout,
                    tool,
                    state(annot_edit::A_COLOR),
                    state(annot_edit::A_WIDTH),
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
        let tool = edit.map_or(Tool::Arrow, |e| e.0);
        h.popup.paint(pal, |ctx| match kind {
            Popup::Opacity => widgets::draw_slider_popup(ctx, strings.opacity, opacity, pal, s),
            Popup::Zoom => {
                let labels: Vec<String> = ZOOMS.iter().map(|v| format!("{v}%")).collect();
                widgets::draw_list(ctx, &labels, zoom_sel(zoom), hover, pw as i32, pal, s);
            }
            Popup::Ann(c) => annot_edit::draw_popup(ctx, c, tool, hover, pw, pal, s),
        });
    });
}

fn tip_text(i: usize, editing: Option<Tool>) -> (&'static str, &'static str) {
    let t = crate::strings();
    if let Some(tool) = editing {
        return annot_edit::tip(i, tool);
    }
    match i {
        I_COPY => (t.copy, "Ctrl+C"),
        I_SAVE => (t.save, "Ctrl+S"),
        I_ANNOTATE => (t.annotate, ""),
        I_OPACITY => (t.opacity, ""),
        I_ZOOM => (t.zoom, ""),
        I_CLICKTHROUGH => (t.clickthrough, ""),
        I_TOPMOST => (t.topmost, ""),
        I_CLOSE => (t.close, "Esc"),
        _ => ("", ""),
    }
}

fn editing_tool(owner: Option<HWND>) -> Option<Tool> {
    owner.and_then(find).and_then(|p| {
        p.try_borrow()
            .ok()
            .and_then(|p| p.edit.as_ref().map(|e| e.tool))
    })
}

fn show_tip() {
    let pal = ui::palette();
    let editing = editing_tool(with_hud(|h| h.owner).flatten());
    with_hud(|h| {
        let Some(i) = h.hover else { return };
        // 開いたポップアップの見出しに重ねない。
        if h.pressed.is_some() || h.open.is_some() {
            return;
        }
        let s = h.s;
        let (name, key) = tip_text(i, editing);
        if name.is_empty() {
            return;
        }
        let tip = Tooltip::new(name, key, s);
        let (layout, _) = toolbar_layout(&h.items, s);
        let (x, bw) = layout[i];
        let top = h.tb.top + px(4.0, s);
        let target = RECT {
            left: h.tb.left + x,
            top,
            right: h.tb.left + x + bw,
            bottom: top + px(32.0, s),
        };
        let at = widgets::place_tooltip(target, tip.w, tip.h, px(8.0, s), h.work);
        h.tooltip.show_at(RECT {
            left: at.x,
            top: at.y,
            right: at.x + tip.w,
            bottom: at.y + tip.h,
        });
        h.tooltip.paint(pal, |ctx| tip.draw(ctx, pal, s));
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
    if let Some(tool) = editing_tool(Some(owner)) {
        use annot_edit::Action;
        let action = annot_edit::action(i, tool);
        if let Some(Action::Tool(t)) = action {
            set_tool(owner, t);
            return;
        }
        finish_text(owner, true);
        match action {
            Some(Action::Popup(c)) => toggle_popup(Popup::Ann(c)),
            Some(Action::Undo) => history(owner, false),
            Some(Action::Redo) => history(owner, true),
            Some(Action::Done) => leave_edit(owner),
            Some(Action::Tool(_)) | None => {}
        }
        return;
    }
    match i {
        I_COPY => copy(owner),
        I_SAVE => save(owner),
        I_ANNOTATE => enter_edit(owner),
        I_OPACITY => toggle_popup(Popup::Opacity),
        I_ZOOM => toggle_popup(Popup::Zoom),
        I_CLICKTHROUGH => set_clickthrough(owner, true),
        I_TOPMOST => {
            let on = find(owner).map(|p| p.borrow().topmost).unwrap_or(true);
            set_topmost(owner, !on);
        }
        I_CLOSE => close(owner),
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
    let in_pin = pin.as_ref().is_some_and(|p| {
        p.try_borrow().is_ok_and(|p| {
            p.edit.is_some()
                || (!p.clickthrough
                    && (contains(&pin_window_rect(p.img, p.s), pt) || p.drag.is_some()))
        })
    });
    let keep = with_hud(|h| {
        in_pin || h.sliding || contains(&h.tb, pt) || (h.open.is_some() && contains(&h.pop, pt))
    })
    .unwrap_or(false);
    if !keep {
        hide_hud();
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
    let open = with_hud(|h| h.open).flatten();
    let choice = open.filter(|&k| k != Popup::Opacity);
    match (msg, which) {
        (WM_MOUSEACTIVATE, _) => LRESULT(MA_NOACTIVATE as isize),
        (WM_SETCURSOR, _) => {
            unsafe {
                SetCursor(LoadCursorW(None, IDC_ARROW).ok());
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
            unsafe {
                let _ = ReleaseCapture();
            }
            let fire = with_hud(|h| {
                let hit = hud_hit(h, p.x, p.y);
                let pressed = h.pressed.take();
                (hit == pressed).then_some(hit).flatten()
            })
            .flatten();
            paint_hud();
            if let Some(i) = fire {
                toolbar_action(i);
            }
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
            let owner = with_hud(|h| h.owner.filter(|_| h.hover == Some(I_ZOOM))).flatten();
            if let Some(owner) = owner.filter(|&o| !is_editing(o))
                && let Some(pin) = find(owner)
            {
                let (z, img) = {
                    let p = pin.borrow();
                    (p.zoom, p.img)
                };
                set_zoom(owner, wheel_zoom(z, delta), zoom_anchor(img));
            }
            LRESULT(0)
        }
        (WM_MOUSEWHEEL, Some(2)) if open == Some(Popup::Opacity) => {
            let delta = (wparam.0 >> 16) as u16 as i16;
            if let Some(owner) = with_hud(|h| h.owner).flatten()
                && let Some(pin) = find(owner)
            {
                let v = pin.borrow().opacity as i32 + if delta > 0 { 10 } else { -10 };
                set_opacity(owner, v.clamp(20, 100) as u32);
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
                // ドラッグとクリックの対象は画像の矩形だけ (クリック透過中は印だけ)。
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
                        } else {
                            edit_cursor(&p)
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
            // クリック透過中にクリックが届くのは印の上だけ。押したら透過を解除する。
            if find(hwnd).is_some_and(|p| p.borrow().clickthrough) {
                set_clickthrough(hwnd, false);
                return LRESULT(0);
            }
            with_hud(|h| {
                h.open = None;
                h.popup.hide();
            });
            paint_hud();
            if is_editing(hwnd)
                && let Some(h) = host(hwnd)
            {
                annot_edit::down(&h);
                return LRESULT(0);
            }
            if let Some(pin) = find(hwnd) {
                let mut p = pin.borrow_mut();
                let c = cursor();
                p.drag = Some(POINT {
                    x: c.x - p.img.left,
                    y: c.y - p.img.top,
                });
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
            let drag = pin.borrow().drag;
            if let Some(off) = drag {
                let c = cursor();
                {
                    let mut p = pin.borrow_mut();
                    let (w, h) = (p.img.right - p.img.left, p.img.bottom - p.img.top);
                    let (l, t) = (c.x - off.x, c.y - off.y);
                    p.img = RECT {
                        left: l,
                        top: t,
                        right: l + w,
                        bottom: t + h,
                    };
                }
                apply_rect(&pin);
                place_hud();
            } else {
                show_hud(hwnd);
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
            }
            if msg == WM_LBUTTONUP {
                unsafe {
                    let _ = ReleaseCapture();
                }
            }
            LRESULT(0)
        }
        // 右クリックのメニューは無い (コピー・保存・閉じるはツールバーにある)。ツールバーを出す。
        WM_RBUTTONUP => {
            show_hud(hwnd);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = (wparam.0 >> 16) as u16 as i16;
            if let Some(pin) = find(hwnd) {
                let z = pin.borrow().zoom;
                set_zoom(hwnd, wheel_zoom(z, delta), cursor());
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
            let editing = is_editing(hwnd);
            match (ctrl, wparam.0 as u16) {
                (true, 0x43) => copy(hwnd),
                (true, 0x53) => save(hwnd),
                (true, 0x5A) if editing => history(hwnd, false),
                (true, 0x59) if editing => history(hwnd, true),
                (false, k) if k == VK_DELETE.0 && editing => {
                    if let Some(h) = host(hwnd) {
                        annot_edit::delete_selected(&h);
                    }
                }
                // ホイールと同じくカーソルを中心に戻す (左上を基準にすると、拡大で左へ伸びたピンが画面外に出る)。
                (true, 0x30) | (true, 0x60) => set_zoom(hwnd, 1.0, cursor()),
                // 編集中の Esc は選択を外し、何も選んでいなければ編集を終える (ピンは閉じない)。
                (false, k) if k == VK_ESCAPE.0 && editing => {
                    if !host(hwnd).is_some_and(|h| annot_edit::deselect(&h)) {
                        leave_edit(hwnd);
                    }
                }
                (false, k) if k == VK_ESCAPE.0 => close(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            if let Some(pin) = find(hwnd) {
                set_scale(&pin, (wparam.0 & 0xFFFF) as u32);
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
            probe_toolbar(hwnd);
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
            if with_hud(|h| h.owner == Some(hwnd)).unwrap_or(false) {
                hide_hud();
                // 閉じたらフェードを待たずにそのコマで消す。
                with_hud(|h| {
                    for a in [&h.toolbar, &h.tooltip, &h.popup] {
                        a.hide_now();
                    }
                });
            }
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_PROBE);
                let _ = KillTimer(Some(hwnd), TIMER_STATUS);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
