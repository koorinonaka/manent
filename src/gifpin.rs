//! GIF ピン: 録画を止めた位置に出す窓 (静止画ピンと同じ作り: DirectComposition の透過窓、物理 px の 1:1、
//! 枠 1 と影、移動・拡縮・閉じる・最前面)。画像の下端に全体バーの帯を重ね、下に acrylic のトレイ
//! (状態の文・削除のアイコンのボタン・フィルムストリップ) を付ける。
//! ホバーで静止画ピンと同じ並びのツールバー (コピー・今のフレーム・保存 / 注釈 / オプション、拡大率 /
//! 最前面 / 閉じる) を出す。オプションは書き出しの寸法のポップアップ (倍率の一覧) を開く。
//! 注釈は静止画ピンと同じエディタ (`annot_edit`) をホストする。注釈ごとに表示の区間 (保存したフレームの添字)
//! を持ち、注釈モードでは帯を画像から外してトレイのいちばん下の段に付ける (トレイは 108)。
//! 書き出しは元のフレームレートのまま、選んだ倍率で: コピーは一時 .gif を CF_HDROP と `GIF` で載せ、保存は .gif を保存する。

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dwm::{
    DWMWA_TRANSITIONS_FORCEDISABLED, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmFlush,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_DELETE, VK_ESCAPE, VK_LEFT, VK_RIGHT,
    VK_SHIFT, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use crate::annot::{self, Annot, Doc, History, Span, Tool};
use crate::annot_edit::{self, Choice, Editor, HostRef, Vis};
use crate::capture::Image;
use crate::clip::{AnnotSeg, FrameStore, GifOptions, scaled_size};
use crate::layout::{pin_margins, pin_window_rect, place_toolbar};
use crate::pin::{ZOOMS, wheel_zoom, zoom_anchor, zoom_label, zoom_sel};
use crate::record::Recorded;
use crate::settings::{GIF_LIMITS, GIF_SCALES};
use crate::timeline::Timeline;
use crate::ui::anim::{self, Anim};
use crate::ui::widgets::status::{self, Band, Kind, Status};
use crate::ui::widgets::timeline::{
    BandGeo, band_geo, bracket_hit, center_scroll, draw_band, draw_strip, follow, frame_at,
    strip_hit, strip_items, strip_width, thumb_x, visible_frames,
};
use crate::ui::widgets::{
    self, Acrylic, BtnState, Capture, Item, Look, Tooltip, draw_icon, draw_toolbar, place_tooltip,
    toolbar_height, toolbar_hit, toolbar_layout, tray_icon_button,
};
use crate::ui::{
    self, Palette, Style, Surface, contains, cursor, draw_text, fill_round, lparam_point, px,
    rectf, rf, stroke, track_leave, work_area,
};

const PIN_CLASS: PCWSTR = w!("OdileGifPin");
const HUD_CLASS: PCWSTR = w!("OdileGifHud");
const TIMER_PLAY: usize = 1;
const TIMER_TIP: usize = 2;
/// ツールバーを残すかの見回り (ピンの窓に張る)。
const TIMER_BAR: usize = 3;
/// ボタンの遷移のコマ (ツールバーとトレイの窓に張る)。
const TIMER_ANIM: usize = 4;
/// エンコードが長引いたらコピーと保存を無効の見た目にする (ピンの窓に張る)。
const TIMER_BUSY: usize = 5;
/// コピー・保存の帯の刻み (進捗の数・出入り・残す時間。ピンの窓に張る)。帯が出ている間だけ回す。
const TIMER_STATUS: usize = 6;
const STATUS_TICK_MS: u32 = 16;
/// 端のスクロールが始まる帯の幅 (DIP)。
const EDGE_PX: f32 = 32.0;
/// 端の帯へ入った深さ d (DIP、窓の外まで続く) からの速さ: `EDGE_SPEED*d + EDGE_ACCEL*d²` (DIP/秒)、上限つき。
/// 帯の中はゆっくり、窓の外へ出るほど急に速くなる。
const EDGE_SPEED: f32 = 4.0;
const EDGE_ACCEL: f32 = 0.6;
/// 帯の外 (窓の外) へ出た分 o (DIP) に足す `EDGE_OUT*o²`。帯の中の速さは変えずに、外だけ速くする。
const EDGE_OUT: f32 = 2.0;
const EDGE_MAX: f32 = 8000.0;
/// 帯の「見えている範囲」と再生位置の遷移の時間。
const VIS_MS: f32 = 90.0;
/// 短いエンコードでは見た目を変えない (変えるとボタンが一瞬点滅する)。
const BUSY_LOOK_MS: u32 = 400;
/// トレイの削除のボタンの遷移の鍵 (ツールバーの添字と被らない)。
const K_DELETE: usize = 100;
const K_DOCK: usize = 101;
/// ピンがオフのとき、カーソルが離れてからタイムライン (トレイとシークバー) を隠すまで。
const TIMELINE_HIDE: Duration = Duration::from_millis(1500);

/// 静止画ピンのツールバーと同じ作り。不透明度・クリック透過は無い。
/// 注釈は静止画ピンの不透明度と同じ位置 (保存の後ろの区切りの右)、オプションはその後ろの区切りの右。
/// 注釈モードではエディタの並び (`annot_edit::items`) に替える。
const ITEMS: [Item; 12] = [
    Item::Tool("copy"),
    Item::Tool("image"),
    Item::Tool("save"),
    Item::Sep,
    Item::Tool("annotate"),
    Item::Sep,
    Item::Tool("sliders"),
    Item::Wide,
    Item::Sep,
    Item::Tool("topmost"),
    Item::Sep,
    Item::Tool("close"),
];
const I_COPY: usize = 0;
const I_FRAME: usize = 1;
const I_SAVE: usize = 2;
const I_ANNOTATE: usize = 4;
const I_OPT: usize = 6;
const I_ZOOM: usize = 7;
const I_TOPMOST: usize = 9;
const I_CLOSE: usize = 11;
/// 別スレッドのエンコードが終わった (`GifPin::encoding` に結果がある)。
const WM_GIF_DONE: u32 = WM_APP + 1;
/// 帯の遷移の 1 コマ (画面の更新ごとに `tick_on_vsync` の糸が投函する)。
const WM_VIS_FRAME: u32 = WM_APP + 2;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Drag {
    /// カーソルから画像の左上までのずれ。
    Move(POINT),
    Seek,
    Select,
    /// トリムの括弧 (true は始め)。
    Trim(bool),
    /// 帯の下の段のつまみ (ストリップのスクロール): 押した x からつまみの中心までのずれ。
    Scroll(i32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TrayHot {
    /// タイムラインを常に表示するピン。
    Dock,
    /// 区間外の注釈のトグル (注釈モードでピンの場所に出す)。
    Ghost,
    Delete,
    Strip,
    /// 選んだ注釈の区間が窓の外にあるときの、窓の端の印。
    Edge,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Tray,
    Tip,
    Bar,
    Pop,
    List,
}

struct GifPin {
    hwnd: HWND,
    surface: Surface,
    /// 録画後は書き換えない。エンコードのスレッドと共有する。
    store: Arc<FrameStore>,
    tl: Timeline,
    thumbs: Vec<Vec<u8>>,
    thumb_size: (u32, u32),
    w: u32,
    h: u32,
    bitmap: ID2D1Bitmap1,
    /// `bitmap` に入っているフレーム。
    shown: usize,
    /// 復元済みのフレームの画素と、その番号。次のフレームは差分だけ当てる。注釈のモザイクの元。
    frame: Image,
    decoded: Option<usize>,
    /// 巻き戻し用に先に復元したフレーム (先頭の番号と画素)。差分は前へしか当てられないため。
    back: Option<(usize, Vec<Vec<u8>>)>,
    /// 最後に描いたタイムラインの状態。同じなら `update` は描かない。
    painted: Option<Painted>,
    /// サムネイルのビットマップ。描くときに初めて作り、以後は使い回す (全 HUD が同じ D2D デバイス)。
    thumb_bmps: RefCell<Vec<Option<ID2D1Bitmap1>>>,
    /// 画面上の画像の矩形 (物理 px、拡縮後)。
    img: RECT,
    zoom: f32,
    s: f32,
    /// 影を描き置いた bitmap (`paint_pin` が大きさと s で作り直す)。
    shadow: RefCell<Option<ui::ShadowBitmap>>,
    drag: Option<Drag>,
    /// 再生中: 直前の刻みの時刻と、使い残した時間。
    playing: Option<(Instant, Duration)>,
    play_hover: bool,
    /// 帯の下の段 (ストリップのスクロールバー) にホバー中。
    rail_hover: bool,
    tray: Acrylic,
    tip: Acrylic,
    bar: Acrylic,
    tray_rect: RECT,
    /// タイムラインを常に表示する (トレイのピン)。オフなら離れて `TIMELINE_HIDE` で隠す。
    dock: bool,
    /// タイムライン (トレイとシークバー) を出している。
    timeline: bool,
    /// カーソルが最後にピン・トレイ・ツールバーの上にいた時刻。
    last_in: Instant,
    scroll: i32,
    tray_hover: Option<TrayHot>,
    tray_pressed: Option<TrayHot>,
    /// ストリップの矩形ドラッグ。
    marquee: Option<Marquee>,
    bar_rect: RECT,
    bar_hover: Option<usize>,
    bar_pressed: Option<usize>,
    /// 書き出しの寸法のポップアップと、倍率の一覧。
    pop: Acrylic,
    list: Acrylic,
    pop_rect: RECT,
    list_rect: RECT,
    pop_open: bool,
    /// `list` の窓をこのドロップダウンの一覧として開いている。
    list_open: Option<Dd>,
    /// `list` の窓を拡大率の一覧 (ツールバーの拡大率のボタンの下) として開いている。
    zoom_open: bool,
    /// この上にいるドロップダウン。
    dd_hover: Option<Dd>,
    list_hover: Option<usize>,
    /// 書き出しの倍率 (%)。`GIF_SCALES` のどれか。
    scale: u32,
    /// 書き出しのファイルサイズの上限 (MB)。`GIF_LIMITS` のどれか。0 は上限なし。
    limit_mb: u32,
    /// ボタンの遷移中の見た目。描く (`&GifPin`) の中で進めるので RefCell。
    anims: RefCell<Anim<(usize, u8)>>,
    /// 帯の遷移のコマを画面の更新ごとに刻む糸の番号。0 は止まっている。
    vsync: Arc<AtomicU32>,
    /// 投函した `WM_VIS_FRAME` がまだ処理されていない。糸は次を投函しない。
    vis_pending: Arc<AtomicBool>,
    topmost: bool,
    /// 端のスクロールの刻み (`edge_pump`) を回している。
    edge_timer: bool,
    /// 端のスクロールの刻みの糸の番号。0 は止まっている。
    edge_gen: Arc<AtomicU32>,
    /// Ctrl+C で作った一時 .gif。ピンを閉じたら消す。
    temp: Vec<PathBuf>,
    /// エンコード中 (busy) の結果の受け口。`Some` の間はコピーと保存を無効にする。
    encoding: Option<mpsc::Receiver<Encoded>>,
    /// エンコードが `BUSY_LOOK_MS` を超えた。コピーと保存を無効の見た目で描く。
    busy_look: bool,
    /// エンコードの進捗 (`pack_progress`)。エンコードのスレッドが書き、帯の刻みが読む。
    progress: Arc<AtomicU64>,
    /// 今のエンコードはコピー (進捗の帯のアイコン)。
    export_copy: bool,
    /// コピー・保存の帯。
    status: Status,
    /// 保存ダイアログが開いている。HUD の窓はダイアログに無効にされないので、自分で押下を捨てる。
    modal: bool,
    /// 注釈 (画像の物理 px)。
    doc: Doc,
    /// 区間と undo の印の列。
    ann: RefCell<Ann>,
    /// 注釈モードの間だけある。
    edit: Option<Editor>,
    /// 区間外の注釈を灰色で出す (トレイのトグル、既定オフ)。注釈モードの間だけ効く。
    ghosts: bool,
    /// 注釈のツールバーの色・線幅・文字の大きさのポップアップ (`pop` の窓を使う) と、そのホバー。
    ann_pop: Option<Choice>,
    ann_hover: Option<usize>,
}

/// 注釈の区間と undo の印の列。エディタは `Host::toolbar_changed` (`&self`) の中で手を知らせるので RefCell。
#[derive(Default)]
struct Ann {
    /// `doc.items` と同じ並び。
    spans: Vec<Span>,
    /// フレームの編集と注釈の undo の印の列。
    hist: History,
    /// 最後に合わせたときの `doc.items` と `doc.depth()`。
    snap: Vec<Annot>,
    depth: usize,
}

/// ストリップの矩形ドラッグ: 押した点 (x はストリップの中身の座標、y はトレイの座標) と今の点 (トレイの座標)。
/// 4 px 動くまでは `moved` が偽 (ただのクリック)。
#[derive(Clone, Copy)]
struct Marquee {
    x0: i32,
    y0: i32,
    cur: POINT,
    moved: bool,
    /// 端のスクロールの前回の時刻と、整数 px に満たない端数。
    last: Instant,
    acc: f32,
}

/// `update` が描いた状態: 今・範囲・トリム・削除の数・再生中・ストリップの矩形ドラッグの今の点。
type Painted = (
    usize,
    Option<(usize, usize)>,
    (usize, usize),
    usize,
    bool,
    Option<(i32, i32)>,
);

thread_local! {
    static PINS: RefCell<Vec<Rc<RefCell<GifPin>>>> = const { RefCell::new(Vec::new()) };
}

fn find(hwnd: HWND) -> Option<Rc<RefCell<GifPin>>> {
    PINS.with(|p| {
        p.try_borrow()
            .ok()?
            .iter()
            .find(|x| x.try_borrow().is_ok_and(|x| x.hwnd == hwnd))
            .cloned()
    })
}

fn find_hud(hwnd: HWND) -> Option<(Rc<RefCell<GifPin>>, Role)> {
    PINS.with(|p| {
        p.try_borrow().ok()?.iter().find_map(|x| {
            let g = x.try_borrow().ok()?;
            let role = if g.tray.hwnd == hwnd {
                Role::Tray
            } else if g.tip.hwnd == hwnd {
                Role::Tip
            } else if g.bar.hwnd == hwnd {
                Role::Bar
            } else if g.pop.hwnd == hwnd {
                Role::Pop
            } else if g.list.hwnd == hwnd {
                Role::List
            } else {
                return None;
            };
            Some((x.clone(), role))
        })
    })
}

fn shift() -> bool {
    unsafe { GetKeyState(VK_SHIFT.0 as i32) < 0 }
}

fn offset(r: RECT, x: i32, y: i32) -> RECT {
    RECT {
        left: r.left + x,
        top: r.top + y,
        right: r.right + x,
        bottom: r.bottom + y,
    }
}

fn register() {
    thread_local!(static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    if !DONE.with(|d| d.replace(true)) {
        widgets::register_class(PIN_CLASS, pin_proc);
        widgets::register_class(HUD_CLASS, hud_proc);
    }
}

/// 録画を `at` (録画した矩形、物理 px) に GIF ピンとして出す。
pub fn create(rec: Recorded, at: RECT) {
    register();
    let (w, h) = (
        (at.right - at.left).max(1) as u32,
        (at.bottom - at.top).max(1) as u32,
    );
    let img = RECT {
        left: at.left,
        top: at.top,
        right: at.left + w as i32,
        bottom: at.top + h as i32,
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
            eprintln!("gifpin: window failed: {e}");
            return;
        }
    };
    let _ = widgets::set_attr(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_DONOTROUND);
    let _ = widgets::set_attr(
        hwnd,
        DWMWA_TRANSITIONS_FORCEDISABLED,
        &windows::core::BOOL::from(true),
    );
    ui::exclude_from_capture(hwnd);
    let destroy = || unsafe {
        let _ = DestroyWindow(hwnd);
    };
    let Ok(surface) = Surface::new(hwnd, 1, 1) else {
        destroy();
        return;
    };
    let first = rec.store.frame(0).bgra;
    let n_thumbs = rec.thumbs.len();
    let Ok(bitmap) = ui::bitmap_from_bgra(&surface.ctx, w, h, &first) else {
        destroy();
        return;
    };
    let huds = (
        Acrylic::new(HUD_CLASS, false, Capture::Setting),
        Acrylic::new(HUD_CLASS, true, Capture::Setting),
        Acrylic::new(HUD_CLASS, false, Capture::Setting),
        Acrylic::new(HUD_CLASS, false, Capture::Setting),
        Acrylic::new(HUD_CLASS, false, Capture::Setting),
    );
    let (Ok(tray), Ok(tip), Ok(bar), Ok(pop), Ok(list)) = huds else {
        eprintln!("gifpin: hud windows failed");
        destroy();
        return;
    };
    let pin = Rc::new(RefCell::new(GifPin {
        hwnd,
        surface,
        tl: Timeline::new(rec.ts, rec.end),
        store: Arc::new(rec.store),
        thumbs: rec.thumbs,
        thumb_size: rec.thumb_size,
        w,
        h,
        bitmap,
        shown: 0,
        frame: Image { w, h, bgra: first },
        decoded: Some(0),
        back: None,
        painted: None,
        thumb_bmps: RefCell::new(vec![None; n_thumbs]),
        img,
        zoom: 1.0,
        s: ui::scale_of(hwnd),
        shadow: RefCell::new(None),
        drag: None,
        playing: None,
        play_hover: false,
        rail_hover: false,
        tray,
        tip,
        bar,
        tray_rect: RECT::default(),
        scroll: 0,
        tray_hover: None,
        tray_pressed: None,
        dock: true,
        timeline: true,
        last_in: Instant::now(),
        marquee: None,
        bar_rect: RECT::default(),
        bar_hover: None,
        bar_pressed: None,
        pop,
        list,
        pop_rect: RECT::default(),
        list_rect: RECT::default(),
        pop_open: false,
        list_open: None,
        zoom_open: false,
        dd_hover: None,
        list_hover: None,
        scale: crate::settings().gif_export_scale,
        limit_mb: crate::settings().gif_export_limit_mb,
        anims: RefCell::new(Anim::default()),
        vsync: Arc::new(AtomicU32::new(0)),
        vis_pending: Arc::new(AtomicBool::new(false)),
        topmost: true,
        edge_timer: false,
        edge_gen: Arc::new(AtomicU32::new(0)),
        temp: Vec::new(),
        encoding: None,
        busy_look: false,
        progress: Arc::new(AtomicU64::new(0)),
        export_copy: false,
        status: Status::default(),
        modal: false,
        doc: Doc::default(),
        ann: RefCell::new(Ann::default()),
        edit: None,
        ghosts: false,
        ann_pop: None,
        ann_hover: None,
    }));
    PINS.with(|p| p.borrow_mut().push(pin.clone()));
    apply_rect(&pin);
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        // 隠れたツールバーの場所のホバーも見るので、出ていなくても回す。
        SetTimer(Some(hwnd), TIMER_BAR, 100, None);
    }
    crate::force_foreground(hwnd);
    let p = pin.borrow();
    eprintln!(
        "gifpin: {}x{} at ({}, {}), {} frames, {:.2} s, tray {}x{} at ({}, {})",
        w,
        h,
        img.left,
        img.top,
        p.tl.len(),
        p.tl.total().as_secs_f64(),
        p.tray_rect.right - p.tray_rect.left,
        p.tray_rect.bottom - p.tray_rect.top,
        p.tray_rect.left,
        p.tray_rect.top
    );
    let dl = offset(tray_layout(&p).del, p.tray_rect.left, p.tray_rect.top);
    eprintln!(
        "gifpin: tray delete ({}, {})-({}, {})",
        dl.left, dl.top, dl.right, dl.bottom
    );
}

// ---- 置き方 ----

fn local_img(p: &GifPin) -> RECT {
    let (ml, mt, _, _) = pin_margins(p.s);
    RECT {
        left: ml,
        top: mt,
        right: ml + p.img.right - p.img.left,
        bottom: mt + p.img.bottom - p.img.top,
    }
}

/// 帯の矩形 (画面の物理 px)。注釈モードではトレイのいちばん下の段。
fn band_of(p: &GifPin) -> (RECT, BandGeo) {
    if let Some(b) = tray_layout(p).band {
        let b = offset(b, p.tray_rect.left, p.tray_rect.top);
        return (b, band_geo(b.right - b.left, p.s));
    }
    let g = band_geo(p.img.right - p.img.left, p.s);
    let band = RECT {
        left: p.img.left,
        top: p.img.bottom - g.h,
        right: p.img.right,
        bottom: p.img.bottom,
    };
    (band, g)
}

/// トレイの中の位置 (トレイの左上から)。
#[derive(Debug, PartialEq)]
struct TrayLayout {
    w: i32,
    h: i32,
    /// タイムラインのピン。注釈モードでは同じ場所が区間外の注釈のトグル。
    dock: RECT,
    del: RECT,
    view: RECT,
    /// 注釈モードのシークバー (帯) の段。通常は画像の下端に重ねるので無い。
    band: Option<RECT>,
}

fn tray_layout(p: &GifPin) -> TrayLayout {
    tray_layout_for(p.img.right - p.img.left, p.s, p.edit.is_some())
}

/// 削除は 22 × 22 のアイコンのボタン。右辺はトレイの右から 8・上から 4。ピンは削除の左 4。
/// 注釈モード (`edit`) は高さ 108 = 状態の段 26 + サムネイルの段 38 + シークバー 44 (GifPinAnnotate のカード)。
fn tray_layout_for(img_w: i32, s: f32, edit: bool) -> TrayLayout {
    let w = img_w.max(px(400.0, s));
    let h = px(if edit { 108.0 } else { 72.0 }, s);
    let bh = px(22.0, s);
    let top = px(4.0, s);
    let dr = w - px(8.0, s);
    let del = RECT {
        left: dr - bh,
        top,
        right: dr,
        bottom: top + bh,
    };
    let dock = offset(del, -(bh + px(4.0, s)), 0);
    let vt = px(26.0, s);
    let view = RECT {
        left: px(8.0, s),
        top: vt,
        right: w - px(8.0, s),
        bottom: vt + px(if edit { 42.0 } else { 46.0 }, s),
    };
    let band = edit.then(|| RECT {
        left: 0,
        top: px(64.0, s),
        right: w,
        bottom: h,
    });
    TrayLayout {
        w,
        h,
        dock,
        del,
        view,
        band,
    }
}

fn status_text(n: usize, d: Duration) -> String {
    status_with(crate::strings().frames_status, n, d)
}

fn status_with(form: &str, n: usize, d: Duration) -> String {
    form.replace("{n}", &n.to_string())
        .replace("{s}", &format!("{:.2}", d.as_secs_f64()))
}

/// 窓を画像の矩形と s に合わせて置き、トレイとポップアップも付いて動かして、描き直す。
fn apply_rect(pin: &Rc<RefCell<GifPin>>) {
    for _ in 0..2 {
        let (hwnd, want) = {
            let mut p = pin.borrow_mut();
            let want = pin_window_rect(p.img, p.s);
            let _ = p.surface.resize(
                (want.right - want.left) as u32,
                (want.bottom - want.top) as u32,
            );
            (p.hwnd, want)
        };
        // 描いてから窓を動かし、すぐ出す (動かしてから描くと、描き終えるまで古い中身が新しい位置に出る)。
        {
            let p = pin.borrow();
            p.surface.hold();
            paint_pin(&p);
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
    place_huds(pin);
    let p = pin.borrow();
    paint_tray(&p);
    if p.bar.visible() {
        paint_bar(&p);
        paint_pop(&p);
        paint_zoom_list(&p);
    }
}

/// s を `dpi` / 96 にする (画像は物理 px のまま)。同じ s なら何もしない。
fn set_scale(pin: &Rc<RefCell<GifPin>>, dpi: u32) {
    let s = dpi as f32 / 96.0;
    let old = std::mem::replace(&mut pin.borrow_mut().s, s);
    if old == s {
        return;
    }
    eprintln!("gifpin: dpi {} -> {dpi}", (old * 96.0).round());
    apply_rect(pin);
}

/// 拡大率がその場で変わった。止まっている GIF ピンは WM_DPICHANGED を受けないので、モニタの DPI を読み直す。
pub fn dpi_changed() {
    let pins: Vec<_> = PINS.with(|p| p.borrow().clone());
    for pin in pins {
        let hwnd = pin.borrow().hwnd;
        set_scale(&pin, crate::monitor_dpi(hwnd));
    }
}

/// トレイを画像の下 8 に左を揃えて置く。ツールバーが出ていれば付いて動かす。
fn place_huds(pin: &Rc<RefCell<GifPin>>) {
    let mut p = pin.borrow_mut();
    let s = p.s;
    let lay = tray_layout(&p);
    let top = p.img.bottom + px(8.0, s);
    p.tray_rect = RECT {
        left: p.img.left,
        top,
        right: p.img.left + lay.w,
        bottom: top + lay.h,
    };
    let tr = p.tray_rect;
    if p.timeline {
        p.tray.show_at(tr);
    }
    if p.bar.visible() {
        place_bar(&mut p);
    }
}

/// タイムライン (トレイとシークバー) を出す / 隠す。
fn set_timeline(pin: &Rc<RefCell<GifPin>>, on: bool) {
    let mut p = pin.borrow_mut();
    if p.timeline == on {
        return;
    }
    p.timeline = on;
    if on {
        let tr = p.tray_rect;
        p.tray.show_at(tr);
        paint_tray(&p);
    } else {
        // トレイは帯の不透明度に付いて薄れ、0 で隠れる (`paint_pin`)。
        p.tray_hover = None;
        p.tip.hide();
    }
    paint_pin(&p);
    eprintln!("gifpin: timeline {}", if on { "shown" } else { "hidden" });
}

/// ピンがオフのとき: カーソルがピンの窓・トレイ・ツールバー・ポップアップにいれば出し、
/// 離れて `TIMELINE_HIDE` たったら隠す。ドラッグ中・再生の操作中は残す。
fn timeline_tick(pin: &Rc<RefCell<GifPin>>) {
    let pt = cursor();
    let (hide, show) = {
        let Ok(mut p) = pin.try_borrow_mut() else {
            return;
        };
        let inside = p.drag.is_some()
            || p.marquee.is_some()
            || p.modal
            || contains(&pin_window_rect(p.img, p.s), pt)
            // 消える途中のトレイに戻れば出し直す。
            || (p.tray.on_screen() && contains(&p.tray_rect, pt))
            || (p.bar.visible() && contains(&p.bar_rect, pt))
            || ((p.pop_open || p.ann_pop.is_some()) && contains(&p.pop_rect, pt))
            || ((p.list_open.is_some() || p.zoom_open) && contains(&p.list_rect, pt));
        if inside {
            p.last_in = Instant::now();
        }
        (
            // 注釈モードではいつも出す (区間を直すのに使う)。
            !p.dock
                && p.edit.is_none()
                && p.timeline
                && !inside
                && p.last_in.elapsed() >= TIMELINE_HIDE,
            !p.timeline && (p.dock || inside),
        )
    };
    if hide {
        set_timeline(pin, false);
    } else if show {
        set_timeline(pin, true);
    }
}

fn set_dock(pin: &Rc<RefCell<GifPin>>, on: bool) {
    {
        let mut p = pin.borrow_mut();
        p.dock = on;
        p.last_in = Instant::now();
        paint_tray(&p);
    }
    eprintln!("gifpin: keep timeline {on}");
}

/// ツールバーを静止画ピンと同じ置き方 (上 8 → 下 8 → 内側の右上 8) で置いて出す。
fn place_bar(p: &mut GifPin) {
    p.bar_rect = bar_rect(p);
    let r = p.bar_rect;
    p.bar.show_at(r);
    if p.pop_open {
        place_pop(p);
    }
    if p.zoom_open {
        place_zoom_list(p);
    }
}

/// 拡大率の一覧をツールバーの下 4 に、左端を拡大率のボタンに揃えて出す (下に入らなければ上 4、
/// 右に入らなければ作業領域に入るまで左へ)。
fn place_zoom_list(p: &mut GifPin) {
    let s = p.s;
    let (layout, _) = toolbar_layout(&ITEMS, s);
    let (lw, lh) = widgets::list_size(ZOOMS.len(), 0, s);
    let work = work_area(p.img);
    let bar = p.bar_rect;
    let x = (bar.left + layout[I_ZOOM].0)
        .min(work.right - lw)
        .max(work.left);
    let below = bar.bottom + px(4.0, s);
    let y = if below + lh <= work.bottom {
        below
    } else {
        bar.top - px(4.0, s) - lh
    };
    p.list_rect = RECT {
        left: x,
        top: y,
        right: x + lw,
        bottom: y + lh,
    };
    let r = p.list_rect;
    p.list.show_at(r);
}

/// 拡大率の一覧を開く / 閉じる。開くと書き出しのポップアップは閉じる。
fn set_zoom_list(p: &mut GifPin, open: bool) {
    if open {
        if p.pop_open {
            set_pop(p, false);
        }
        p.zoom_open = true;
        p.list_hover = zoom_sel(p.zoom);
        place_zoom_list(p);
        paint_zoom_list(p);
    } else if p.zoom_open {
        p.zoom_open = false;
        p.list_hover = None;
        p.list.hide();
    }
}

fn paint_zoom_list(p: &GifPin) {
    if !p.zoom_open {
        return;
    }
    let pal = ui::palette();
    let labels: Vec<String> = ZOOMS.iter().map(|v| format!("{v}%")).collect();
    let w = p.list_rect.right - p.list_rect.left;
    p.list.paint(pal, |ctx| {
        widgets::draw_list(ctx, &labels, zoom_sel(p.zoom), p.list_hover, w, pal, p.s)
    });
}

/// 書き出しのポップアップのドロップダウン。値は `pop_layout` の矩形の添字。
#[derive(Clone, Copy, PartialEq, Debug)]
enum Dd {
    Scale = 0,
    Limit = 1,
}

impl Dd {
    fn values(self) -> &'static [u32] {
        match self {
            Dd::Scale => &GIF_SCALES,
            Dd::Limit => &GIF_LIMITS,
        }
    }

    fn current(self, p: &GifPin) -> u32 {
        match self {
            Dd::Scale => p.scale,
            Dd::Limit => p.limit_mb,
        }
    }

    fn label(self, v: u32) -> String {
        match (self, v) {
            (Dd::Scale, v) => format!("{v}%"),
            (Dd::Limit, 0) => crate::strings().export_limit_off.into(),
            (Dd::Limit, v) => format!("{v} MB"),
        }
    }
}

/// 書き出しのポップアップの大きさと、その中のドロップダウン (ポップアップの左上から、倍率・サイズ上限の順)。
/// 248 × 134。ドロップダウンは 144 × 32 で、右端は右から 16、上から 42 (見出しの上 14 + 16 + 12) と、その下 12。
fn pop_layout(s: f32) -> (i32, i32, [RECT; 2]) {
    let (w, h) = (px(248.0, s), px(134.0, s));
    let right = w - px(16.0, s);
    let dd = |top: i32| RECT {
        left: right - px(144.0, s),
        top,
        right,
        bottom: top + px(32.0, s),
    };
    (w, h, [dd(px(42.0, s)), dd(px(86.0, s))])
}

/// ポップアップをツールバーの下 4 に、左端をオプションのボタンに揃えて出す (下に入らなければ上 4、
/// 右に入らなければ作業領域に入るまで左へ)。一覧を開いていれば、ドロップダウンの下 4 に右端を揃えて出す。
fn place_pop(p: &mut GifPin) {
    let s = p.s;
    let (layout, _) = toolbar_layout(&items(p), s);
    let work = work_area(p.img);
    let bar = p.bar_rect;
    // 注釈の色・線幅・文字の大きさ: 開いたボタンの下 4、左端を揃える (下に入らなければ上 4)。
    if let Some(c) = p.ann_pop {
        let (anchor, pw, ph) = annot_edit::popup_geometry(c, s);
        let x = (bar.left + layout[anchor].0)
            .min(work.right - pw)
            .max(work.left);
        let below = bar.bottom + px(4.0, s);
        let y = if below + ph <= work.bottom {
            below
        } else {
            bar.top - px(4.0, s) - ph
        };
        p.pop_rect = RECT {
            left: x,
            top: y,
            right: x + pw,
            bottom: y + ph,
        };
        let r = p.pop_rect;
        p.pop.show_at(r);
        return;
    }
    let (pw, ph, dd) = pop_layout(s);
    let x = (bar.left + layout[I_OPT].0)
        .min(work.right - pw)
        .max(work.left);
    let below = bar.bottom + px(4.0, s);
    let y = if below + ph <= work.bottom {
        below
    } else {
        bar.top - px(4.0, s) - ph
    };
    p.pop_rect = RECT {
        left: x,
        top: y,
        right: x + pw,
        bottom: y + ph,
    };
    let r = p.pop_rect;
    p.pop.show_at(r);
    if let Some(k) = p.list_open {
        let dd = dd[k as usize];
        let (lw, lh) = widgets::list_size(k.values().len(), dd.right - dd.left, s);
        let right = x + dd.right;
        let below = y + dd.bottom + px(4.0, s);
        let top = if below + lh <= work.bottom {
            below
        } else {
            y + dd.top - px(4.0, s) - lh
        };
        p.list_rect = RECT {
            left: right - lw,
            top,
            right,
            bottom: top + lh,
        };
        let r = p.list_rect;
        p.list.show_at(r);
    }
}

/// ツールバーの並び: 注釈モードはエディタの並び。
fn items(p: &GifPin) -> Vec<Item> {
    if p.edit.is_some() {
        annot_edit::items()
    } else {
        ITEMS.to_vec()
    }
}

fn bar_rect(p: &GifPin) -> RECT {
    let s = p.s;
    let fw = stroke(1.0, s);
    let frame = RECT {
        left: p.img.left - fw,
        top: p.img.top - fw,
        right: p.img.right + fw,
        bottom: p.img.bottom + fw,
    };
    let (_, w) = toolbar_layout(&items(p), s);
    let h = toolbar_height(s);
    let at = place_toolbar(frame, p.img, w, h, px(8.0, s), px(8.0, s), work_area(p.img));
    RECT {
        left: at.x,
        top: at.y,
        right: at.x + w,
        bottom: at.y + h,
    }
}

// ---- 描画 ----

fn paint_all(pin: &Rc<RefCell<GifPin>>) {
    let p = pin.borrow();
    paint_pin(&p);
    paint_tray(&p);
    if p.bar.visible() {
        paint_bar(&p);
        paint_pop(&p);
        paint_zoom_list(&p);
    }
}

fn visible(p: &GifPin) -> Option<(usize, usize)> {
    let lay = tray_layout(p);
    let items = strip_items(&p.tl, p.s);
    visible_frames(&items, p.scroll, lay.view.right - lay.view.left)
}

/// 遷移の間、画面の更新 (DwmFlush) ごとに `WM_VIS_FRAME` を投函する。WM_TIMER は 15.6 ms 刻みでカクつく。
/// `vsync` は今の糸の番号 (0 は止まっている)。遷移が終われば `paint_pin` が 0 にし、糸は次の更新で抜ける。
/// 止めてすぐ次の遷移が始まると新しい番号の糸を出し、古い糸は番号が変わったのを見て抜ける。
/// 番号が無いと古い糸が残って糸が増え、投函が列を埋めて入力と WM_TIMER が届かなくなる
/// (2026-11-06、端のスクロールで遷移が毎コマ止まり・始まり、4 ms ごとに投函され、ボタンを離しても止まらなかった)。
fn tick_on_vsync(p: &GifPin) {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    if p.vsync.load(Ordering::Acquire) != 0 {
        return;
    }
    let mine = NEXT.fetch_add(1, Ordering::Relaxed).max(1);
    p.vsync.store(mine, Ordering::Release);
    let (run, pending, hwnd) = (p.vsync.clone(), p.vis_pending.clone(), p.hwnd.0 as usize);
    std::thread::spawn(move || {
        while run.load(Ordering::Acquire) == mine {
            unsafe {
                let _ = DwmFlush();
                // 前の 1 コマが未処理なら投函しない: 再生は遷移と違い続くので、描くのが更新に追い付かないと
                // 列が埋まり、WM_TIMER (再生の刻み) が遅れる。
                if pending.swap(true, Ordering::AcqRel) {
                    if !IsWindow(Some(HWND(hwnd as _))).as_bool() {
                        break;
                    }
                    continue;
                }
                // 窓が閉じていれば投函に失敗する。
                if PostMessageW(Some(HWND(hwnd as _)), WM_VIS_FRAME, WPARAM(0), LPARAM(0)).is_err()
                {
                    pending.store(false, Ordering::Release);
                    break;
                }
            }
        }
    });
}

/// 帯の見えている範囲・再生位置・選んだ範囲・不透明度の、遷移の途中の値。遷移が残っていればコマを刻む。
type BandAnim = (Option<(f32, f32)>, f32, Option<(f32, f32)>, f32);

fn band_anim(p: &GifPin) -> BandAnim {
    {
        let mut an = p.anims.borrow_mut();
        // 端のスクロール中は遷移させない: 画面の更新ごとの投函 (279 Hz のモニタで 3.9 ms ごと) が
        // UI スレッドを埋め、端のスクロールの刻みが 14〜114 ms に散ってガタつく。範囲は毎刻み動くので跳ばない。
        let m = anim::motion() && !p.edge_timer;
        // 帯のドラッグ中 (シーク・範囲・トリム・つまみ) も遷移させない: ポインタに遅れて付いてくる。
        let m = m
            && !matches!(
                p.drag,
                Some(Drag::Seek | Drag::Select | Drag::Trim(_) | Drag::Scroll(_))
            );
        let vis = visible(p).map(|(a, b)| {
            (
                an.to((usize::MAX, 0), a as f32, VIS_MS, m),
                an.to((usize::MAX, 1), b as f32, VIS_MS, m),
            )
        });
        // 再生中は遷移しない (遅れて見える)。代わりに今のフレームを出してからの時間で次のフレームへ滑らせる:
        // 刻み (フレームの切り替わり) だけで描くと 1 コマずつ跳んでカクつく。
        // 一周で戻る所と削除した区間の手前では滑らせない (戻る・区間の中を通る)。
        let cur = p.tl.cur;
        let glide = p.playing.filter(|_| anim::motion()).map(|(last, carry)| {
            if p.tl.next_play(cur) != cur + 1 {
                return 0.0;
            }
            let own = p.tl.own(cur).as_secs_f32();
            let t = (last.elapsed() + carry).as_secs_f32();
            if own > 0.0 { (t / own).min(1.0) } else { 0.0 }
        });
        let head = an.to(
            (usize::MAX, 2),
            cur as f32 + glide.unwrap_or(0.0),
            VIS_MS,
            m && p.playing.is_none(),
        );
        // 選んだ範囲の両端。消えたら鍵を捨て、次に出た範囲はその場から始める。
        let sel = match p.tl.sel {
            Some((a, b)) => Some((
                an.to((usize::MAX, 3), a as f32, VIS_MS, m),
                an.to((usize::MAX, 4), b as f32, VIS_MS, m),
            )),
            None => {
                an.retain(|k| !matches!(k, (usize::MAX, 3 | 4)));
                None
            }
        };
        // タイムラインの出入り (常に表示がオフのとき): 出すのは速く、消すのはゆっくり薄める。
        let (to, ms) = if p.timeline {
            (1.0, anim::SHOW_MS)
        } else {
            (0.0, anim::FADE_OUT_MS)
        };
        let alpha = an.to((usize::MAX, 5), to, ms, anim::motion());
        if an.busy() || glide.is_some() {
            tick_on_vsync(p);
        } else {
            p.vsync.store(0, Ordering::Release);
        }
        (vis, head, sel, alpha)
    }
}

fn paint_pin(p: &GifPin) {
    crate::insight_scope!("gif_paint_pin");
    let s = p.s;
    let local = local_img(p);
    let image = rf(local);
    let (_, g) = band_of(p);
    // 見えている範囲と再生位置は滑らかに動く。注釈モードの帯はトレイが描く。
    let anim = p.edit.is_none().then(|| band_anim(p));
    // タイムラインを隠す途中: トレイを帯と同じ不透明度にする。画面の更新ごとに進む (WM_TIMER は再生中に遅れる)。
    if let Some(a) = anim
        .map(|a| a.3)
        .filter(|_| !p.timeline && p.tray.visible())
    {
        if a <= 0.0 {
            p.tray.hide_now();
            p.tray.set_alpha(1.0);
        } else {
            p.tray.set_alpha(a);
        }
    }
    let play_state = if p.play_hover {
        BtnState::Hover
    } else {
        BtnState::Normal
    };
    p.surface.draw(|ctx| unsafe {
        // shadow-pin 0 12 34 .42: 標準偏差 17、下へ 12。画像の下には落とさない。
        ui::cached_shadow(
            ctx,
            &p.shadow,
            image,
            17.0 * s,
            ui::rgba(0, 0, 0, 0.42),
            (0.0, px(12.0, s) as f32),
            1.0,
        );
        let fw = stroke(1.0, s) as f32;
        ui::stroke_rect(
            ctx,
            rectf(
                image.left - fw / 2.0,
                image.top - fw / 2.0,
                image.right + fw / 2.0,
                image.bottom + fw / 2.0,
            ),
            fw,
            ui::PIN_FRAME,
        );
        let interp = if p.zoom >= 1.0 {
            D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR
        } else {
            D2D1_INTERPOLATION_MODE_LINEAR
        };
        ctx.DrawBitmap(&p.bitmap, Some(&image), 1.0, interp, None, None);
        annot_edit::draw(ctx, p, local, 1.0);
        if let Some((vis, head, sel, alpha)) = anim.filter(|a| a.3 > 0.0) {
            draw_seek_band(
                ctx,
                p,
                image,
                local,
                g.h,
                (vis, head, sel, alpha),
                play_state,
            );
        }
        // コピー・保存の帯は最後に描く (低い画像ではシークバーの帯の手前に重なる)。注釈モードでも出す。
        p.status.draw(ctx, image, s);
    });
}

/// 画像の下端の全体バーの帯。帯は画像の下端から上へ 44。画像の外には描かない。
fn draw_seek_band(
    ctx: &ID2D1DeviceContext,
    p: &GifPin,
    image: D2D_RECT_F,
    local: RECT,
    band_h: i32,
    (vis, head, sel, alpha): BandAnim,
    play_state: BtnState,
) {
    let s = p.s;
    unsafe {
        ctx.PushAxisAlignedClip(&image, D2D1_ANTIALIAS_MODE_ALIASED);
        let fading = alpha < 1.0;
        if fading {
            ctx.PushLayer(
                &D2D1_LAYER_PARAMETERS1 {
                    // 帯の範囲だけ (画像全体の層は大きい GIF で描画を倍にし、再生の刻みを飢えさせた)。
                    contentBounds: rectf(
                        image.left,
                        image.bottom - band_h as f32,
                        image.right,
                        image.bottom,
                    ),
                    maskTransform: ui::identity(),
                    opacity: alpha,
                    ..Default::default()
                },
                None,
            );
        }
        draw_band(
            ctx,
            image.left,
            image.bottom - band_h as f32,
            local.right - local.left,
            &p.tl,
            p.playing.is_some(),
            play_state,
            vis,
            rail_hot(p),
            head,
            sel,
            None,
            None,
            s,
        );
        if fading {
            ctx.PopLayer();
        }
        ctx.PopAxisAlignedClip();
    }
}

fn paint_tray(p: &GifPin) {
    crate::insight_scope!("gif_paint_tray");
    let pal = ui::palette();
    let s = p.s;
    let lay = tray_layout(p);
    let items = strip_items(&p.tl, s);
    let span = ann_span(p);
    let status = match span {
        // 注釈を選んでいる: 実効の区間 (削除とトリムを除く) の枚数と秒。
        Some(sp) => {
            let (n, d) =
                p.tl.effective(sp)
                    .fold((0, Duration::ZERO), |(n, d), j| (n + 1, d + p.tl.own(j)));
            status_with(crate::strings().span_status, n, d)
        }
        None => {
            let (n, d) = p.tl.status();
            status_text(n, d)
        }
    };
    let edit = p.edit.is_some();
    let band = lay.band.map(|b| (b, band_anim(p)));
    let edge = edge_mark(p);
    let state = |hot: TrayHot, disabled: bool| {
        let on = (hot == TrayHot::Dock && p.dock) || (hot == TrayHot::Ghost && p.ghosts);
        let pressed = p.tray_pressed == Some(hot);
        let hover = p.tray_hover == Some(hot);
        match () {
            _ if disabled => BtnState::Disabled,
            _ if on && pressed => BtnState::OnPressed,
            _ if on && hover => BtnState::OnHover,
            _ if on => BtnState::On,
            _ if pressed => BtnState::Pressed,
            _ if hover => BtnState::Hover,
            _ => BtnState::Normal,
        }
    };
    let (tw_, th_) = p.thumb_size;
    p.tray.paint(pal, |ctx| {
        let row = px(30.0, s) as f32;
        let st = ui::text(&status, Style::NumStrong, s);
        draw_text(
            ctx,
            &st,
            px(12.0, s) as f32,
            ((row - st.h) / 2.0).round(),
            pal.text_primary,
        );
        let (left_icon, left_hot) = if edit {
            ("onion", TrayHot::Ghost)
        } else {
            ("topmost", TrayHot::Dock)
        };
        tray_icon_button(
            ctx,
            lay.dock.left as f32,
            lay.dock.top as f32,
            left_icon,
            eased(p, p.tray.hwnd, K_DOCK, state(left_hot, false)),
            pal,
            s,
        );
        tray_icon_button(
            ctx,
            lay.del.left as f32,
            lay.del.top as f32,
            "trash",
            eased(
                p,
                p.tray.hwnd,
                K_DELETE,
                state(TrayHot::Delete, !can_delete(p)),
            ),
            pal,
            s,
        );
        let thumb = |i: usize| -> Option<ID2D1Bitmap1> {
            let mut cache = p.thumb_bmps.borrow_mut();
            let slot = cache.get_mut(i)?;
            if slot.is_none() {
                let px_ = p.thumbs.get(i)?;
                *slot = (px_.len() == (tw_ * th_ * 4) as usize)
                    .then(|| ui::bitmap_from_bgra(ctx, tw_, th_, px_).ok())
                    .flatten();
            }
            slot.clone()
        };
        // 矩形ドラッグの枠は描かない: 選んだコマの見た目だけで足りる。
        draw_strip(
            ctx,
            rf(lay.view),
            &items,
            p.scroll,
            &p.tl,
            span,
            &thumb,
            pal,
            s,
        );
        if let Some((r, right, f)) = edge {
            draw_edge_mark(ctx, r, right, f, pal, s);
        }
        if let Some((b, (vis, head, sel, _))) = band {
            draw_band(
                ctx,
                0.0,
                b.top as f32,
                lay.w,
                &p.tl,
                p.playing.is_some(),
                if p.play_hover {
                    BtnState::Hover
                } else {
                    BtnState::Normal
                },
                vis,
                rail_hot(p),
                head,
                sel,
                span,
                Some(pal),
                s,
            );
        }
    });
}

/// 選んだ注釈の区間 (保存したフレームの添字)。ストリップのドラッグ中は作っている範囲。
fn ann_span(p: &GifPin) -> Option<(usize, usize)> {
    let k = p.edit.as_ref()?.sel()?;
    if p.marquee.is_some_and(|m| m.moved)
        && let Some(r) = p.tl.sel
    {
        return Some(r);
    }
    p.ann.try_borrow().ok()?.spans.get(k).copied()
}

/// 削除のボタンが効くか: 注釈を選んでいる (注釈を消す) か、範囲がある (フレームを消す)。
fn can_delete(p: &GifPin) -> bool {
    p.edit.as_ref().is_some_and(Editor::any_sel) || p.tl.sel.is_some()
}

/// 窓の端の印 (トレイの座標)、向き (true は右)、押すと送る先のフレーム。選んだ注釈の区間が丸ごと
/// ストリップの窓の外にあるときだけ。高さ 16、左右の padding 6 と 4 (向きの側が 4)、番号と chevron 12 を gap 2。
/// 縦の中央は窓の上から 14。
fn edge_mark(p: &GifPin) -> Option<(RECT, bool, usize)> {
    let (a, b) = ann_span(p)?;
    let s = p.s;
    let lay = tray_layout(p);
    let items = strip_items(&p.tl, s);
    let (first, last) = visible_frames(&items, p.scroll, lay.view.right - lay.view.left)?;
    let (right, f) = if a > last {
        (true, a)
    } else if b < first {
        (false, b)
    } else {
        return None;
    };
    let t = ui::text(&(f + 1).to_string(), Style::NumSmallStrong, s);
    let w = px(6.0 + 2.0 + 12.0 + 4.0, s) + t.w.ceil() as i32;
    let h = px(16.0, s);
    let top = lay.view.top + px(14.0, s) - h / 2;
    let left = if right {
        lay.view.right - w
    } else {
        lay.view.left
    };
    Some((
        RECT {
            left,
            top,
            right: left + w,
            bottom: top + h,
        },
        right,
        f,
    ))
}

fn draw_edge_mark(ctx: &ID2D1DeviceContext, r: RECT, right: bool, f: usize, pal: &Palette, s: f32) {
    let r = rf(r);
    fill_round(ctx, r, px(4.0, s) as f32, pal.on_fill);
    let t = ui::text(&(f + 1).to_string(), Style::NumSmallStrong, s);
    let ch = px(12.0, s) as f32;
    let (pad_n, pad_c, gap) = (px(6.0, s) as f32, px(4.0, s) as f32, px(2.0, s) as f32);
    let (tx, cx) = if right {
        (r.left + pad_n, r.left + pad_n + t.w + gap)
    } else {
        (r.right - pad_n - t.w, r.left + pad_c)
    };
    let mid = (r.top + r.bottom) / 2.0;
    draw_text(
        ctx,
        &t,
        tx.round(),
        (mid - t.h / 2.0).round(),
        pal.on_content,
    );
    // `chevron-down` を回して左右に向ける (D2D の角度は時計回り)。
    let cy = (mid - ch / 2.0).round();
    let turn = windows_numerics::Matrix3x2::rotation_around(
        if right { -90.0 } else { 90.0 },
        windows_numerics::Vector2 {
            X: cx + ch / 2.0,
            Y: cy + ch / 2.0,
        },
    );
    unsafe {
        let mut old = ui::identity();
        ctx.GetTransform(&mut old);
        ctx.SetTransform(&(turn * old));
        draw_icon(ctx, "chevron-down", cx, cy, ch, pal.on_content);
        ctx.SetTransform(&old);
    }
}

fn paint_bar(p: &GifPin) {
    crate::insight_scope!("gif_paint_bar");
    if let Some(e) = &p.edit {
        paint_ann_bar(p, e.tool);
        return;
    }
    let pal = ui::palette();
    let busy = p.busy_look || p.modal;
    let state = |i: usize| {
        let hover = p.bar_hover == Some(i);
        let pressed = p.bar_pressed == Some(i) && hover;
        let on = i == I_TOPMOST && p.topmost;
        match () {
            _ if busy && matches!(i, I_COPY | I_SAVE) => BtnState::Disabled,
            _ if i == I_OPT && p.pop_open => BtnState::Open,
            _ if i == I_ZOOM && p.zoom_open => BtnState::Open,
            _ if on && pressed => BtnState::OnPressed,
            _ if on && hover => BtnState::OnHover,
            _ if on => BtnState::On,
            _ if pressed => BtnState::Pressed,
            _ if hover => BtnState::Hover,
            _ => BtnState::Normal,
        }
    };
    let looks: Vec<Look> = (0..ITEMS.len())
        .map(|i| eased(p, p.bar.hwnd, i, state(i)))
        .collect();
    let s = p.s;
    p.bar.paint(pal, |ctx| {
        draw_toolbar(ctx, &ITEMS, |i| looks[i], pal, s);
        let (layout, _) = toolbar_layout(&ITEMS, s);
        let top = px(4.0, s) as f32;
        widgets::label_button(
            ctx,
            layout[I_ZOOM].0 as f32,
            top,
            &zoom_label(p.zoom),
            looks[I_ZOOM],
            pal,
            s,
        );
    });
}

/// 注釈のツールバー (静止画ピンと同じ並び)。元に戻す・やり直すの可否は undo の印の列から。
fn paint_ann_bar(p: &GifPin, tool: Tool) {
    let pal = ui::palette();
    let s = p.s;
    let items = annot_edit::items();
    let (undo, redo) = p
        .ann
        .try_borrow()
        .map_or((false, false), |a| (a.hist.can_undo(), a.hist.can_redo()));
    let state = |i: usize| {
        let hover = p.bar_hover == Some(i);
        let pressed = p.bar_pressed == Some(i) && hover;
        match annot_edit::button(i, tool, undo, redo, p.ann_pop) {
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
    let looks: Vec<Look> = (0..items.len())
        .map(|i| eased(p, p.bar.hwnd, i, state(i)))
        .collect();
    p.bar.paint(pal, |ctx| {
        draw_toolbar(ctx, &items, |i| looks[i], pal, s);
        let (layout, _) = toolbar_layout(&items, s);
        annot_edit::draw_slots(
            ctx,
            &layout,
            tool,
            state(annot_edit::A_COLOR),
            state(annot_edit::A_WIDTH),
            pal,
            s,
        );
    });
}

/// 書き出しの寸法のポップアップ (と開いていれば一覧) を描く。
/// 見出しは「書き出しの寸法」(`caption`) と書き出し後の寸法 (`num`)、下の段に「倍率」(`body`) とドロップダウン。
fn paint_pop(p: &GifPin) {
    if let Some(c) = p.ann_pop {
        let pal = ui::palette();
        let tool = p.edit.as_ref().map_or(Tool::Arrow, |e| e.tool);
        let pw = (p.pop_rect.right - p.pop_rect.left) as f32;
        p.pop.paint(pal, |ctx| {
            annot_edit::draw_popup(ctx, c, tool, p.ann_hover, pw, pal, p.s)
        });
        return;
    }
    if !p.pop_open {
        return;
    }
    crate::insight_scope!("gif_paint_pop");
    let pal = ui::palette();
    let t = crate::strings();
    let s = p.s;
    let (pw, _, dds) = pop_layout(s);
    let (w2, h2) = scaled_size(p.w, p.h, p.scale);
    p.pop.paint(pal, |ctx| {
        let pad = px(16.0, s) as f32;
        let y = px(14.0, s) as f32;
        let title = ui::text(t.export_size, Style::Caption, s);
        draw_text(ctx, &title, pad, y, pal.text_secondary);
        let v = ui::text(&format!("{w2} × {h2}"), Style::Num, s);
        draw_text(ctx, &v, pw as f32 - pad - v.w, y, pal.text_secondary);
        for (k, name) in [(Dd::Scale, t.export_scale), (Dd::Limit, t.export_limit)] {
            let dd = dds[k as usize];
            let row = ui::text(name, Style::Body, s);
            let cy = dd.top as f32 + (((dd.bottom - dd.top) as f32 - row.h) / 2.0).round();
            draw_text(ctx, &row, pad, cy, pal.text_primary);
            let open = p.dd_hover == Some(k) || p.list_open == Some(k);
            let hot = if open { 1.0 } else { 0.0 };
            widgets::dropdown(ctx, rf(dd), &k.label(k.current(p)), hot, pal, s);
        }
    });
    if let Some(k) = p.list_open {
        let labels: Vec<String> = k.values().iter().map(|&v| k.label(v)).collect();
        let sel = k.values().iter().position(|&v| v == k.current(p));
        let w = p.list_rect.right - p.list_rect.left;
        p.list.paint(pal, |ctx| {
            widgets::draw_list(ctx, &labels, sel, p.list_hover, w, pal, s)
        });
    }
}

/// ポップアップを開く / 閉じる。開け閉めのどちらでも一覧は閉じる。
fn set_pop(p: &mut GifPin, open: bool) {
    p.pop_open = open;
    p.list_open = None;
    p.zoom_open = false;
    p.dd_hover = None;
    p.list_hover = None;
    p.list.hide();
    if !open {
        p.pop.hide();
        eprintln!("gifpin: export popup closed");
        return;
    }
    place_pop(p);
    paint_pop(p);
    let (r, b) = (p.pop_rect, p.bar_rect);
    let dd = pop_layout(p.s).2[Dd::Scale as usize];
    let (w2, h2) = scaled_size(p.w, p.h, p.scale);
    eprintln!(
        "gifpin: export popup {}x{} at bar+({}, {}), bar {}x{}, dropdown {}x{} at ({}, {}), scale {}% -> {w2}x{h2}, limit {} MB",
        r.right - r.left,
        r.bottom - r.top,
        r.left - b.left,
        r.top - b.top,
        b.right - b.left,
        b.bottom - b.top,
        dd.right - dd.left,
        dd.bottom - dd.top,
        dd.left,
        dd.top,
        p.scale,
        p.limit_mb
    );
}

/// 注釈の色・線幅・文字の大きさのポップアップを開く (`Some`) / 閉じる。
fn set_ann_pop(p: &mut GifPin, c: Option<Choice>) {
    p.ann_pop = c;
    p.ann_hover = None;
    if c.is_some() {
        p.tip.hide();
        place_pop(p);
        paint_pop(p);
    } else {
        p.pop.hide();
    }
}

/// ドロップダウンの一覧を開く (`Some`) / 閉じる。
fn set_list(p: &mut GifPin, open: Option<Dd>) {
    p.list_open = open;
    if let Some(k) = open {
        p.list_hover = k.values().iter().position(|&v| v == k.current(p));
        place_pop(p);
        let (r, pr) = (p.list_rect, p.pop_rect);
        eprintln!(
            "gifpin: {k:?} list {}x{} at pop+({}, {})",
            r.right - r.left,
            r.bottom - r.top,
            r.left - pr.left,
            r.top - pr.top
        );
    } else {
        p.list_hover = None;
        p.list.hide();
    }
    paint_pop(p);
}

/// 一覧で倍率かサイズ上限を選んだ: 一覧を閉じて描き直し、次の GIF ピンのために設定へ残す。
fn choose_export(pin: &Rc<RefCell<GifPin>>, k: Dd, v: u32) {
    {
        let mut p = pin.borrow_mut();
        match k {
            Dd::Scale => p.scale = v,
            Dd::Limit => p.limit_mb = v,
        }
        set_list(&mut p, None);
        let (w2, h2) = scaled_size(p.w, p.h, p.scale);
        eprintln!(
            "gifpin: export scale {}% -> {w2}x{h2}, limit {} MB",
            p.scale, p.limit_mb
        );
    }
    crate::update_settings(|s| match k {
        Dd::Scale => s.gif_export_scale = v,
        Dd::Limit => s.gif_export_limit_mb = v,
    });
}

/// ボタン `key` の途中の見た目。遷移が残っていれば `hwnd` にコマのタイマーを張る。
fn eased(p: &GifPin, hwnd: HWND, key: usize, target: BtnState) -> Look {
    let mut a = p.anims.borrow_mut();
    let look = Look::eased(target, key, &mut a, anim::motion());
    if a.busy() {
        unsafe {
            SetTimer(Some(hwnd), TIMER_ANIM, 15, None);
        }
    }
    look
}

/// 削除 (「削除 Delete」) のツールチップ。
fn show_tray_tip(pin: &Rc<RefCell<GifPin>>) {
    let pal: &Palette = ui::palette();
    let t = crate::strings();
    let mut p = pin.borrow_mut();
    if p.tray_pressed.is_some() {
        return;
    }
    let s = p.s;
    let lay = tray_layout(&p);
    let annot = p.edit.as_ref().is_some_and(Editor::any_sel);
    let (tip, btn) = match p.tray_hover {
        Some(TrayHot::Delete) if annot => (Tooltip::new(t.delete_annot, "Delete", s), lay.del),
        Some(TrayHot::Delete) => (Tooltip::new(t.delete, "Delete", s), lay.del),
        Some(TrayHot::Dock) => (Tooltip::new(t.keep_timeline, "", s), lay.dock),
        Some(TrayHot::Ghost) => (Tooltip::new(t.ghost_annots, "", s), lay.dock),
        _ => return,
    };
    let target = offset(btn, p.tray_rect.left, p.tray_rect.top);
    let at = place_tooltip(target, tip.w, tip.h, px(8.0, s), work_area(p.tray_rect));
    p.tip.show_at(RECT {
        left: at.x,
        top: at.y,
        right: at.x + tip.w,
        bottom: at.y + tip.h,
    });
    p.tip.paint(pal, |ctx| tip.draw(ctx, pal, s));
}

// ---- 操作 ----

/// タイムラインが変わった: 今のフレームを復元し、ストリップを今のフレームに合わせて、全部描き直す。
fn update(pin: &Rc<RefCell<GifPin>>) {
    crate::insight_scope!("gif_update");
    {
        let mut p = pin.borrow_mut();
        let cur = p.tl.cur;
        // 再生の刻み (15 ms) は録画の 1 コマ (30 fps で 33 ms) より短く、シークのドラッグも同じフレームの上を動く。
        // 何も変わっていなければ描かない。
        let sig = (
            cur,
            p.tl.sel,
            p.tl.trim,
            p.tl.deleted.len(),
            p.playing.is_some(),
            p.marquee.map(|m| (m.cur.x, m.cur.y)),
        );
        if p.shown == cur && p.painted == Some(sig) {
            return;
        }
        p.painted = Some(sig);
        if p.shown != cur {
            {
                crate::insight_scope!("gif_decode");
                let backward = p.decoded.is_some_and(|d| cur < d);
                if backward
                    && !p
                        .back
                        .as_ref()
                        .is_some_and(|(a, v)| (*a..*a + v.len()).contains(&cur))
                {
                    // 前のフレームはキーフレームから当て直すしかない。1 度に数枚ぶん復元して続く巻き戻しに使う。
                    // ponytail: 64 MB 固定。大画面で足りなければ間引きの基準点に替える
                    let n = (64 << 20) / p.frame.bgra.len().max(1);
                    p.back = Some(p.store.frames_back(cur, n));
                }
                let q = &mut *p;
                match &q.back {
                    Some((a, v)) if (*a..*a + v.len()).contains(&cur) => {
                        q.frame.bgra.copy_from_slice(&v[cur - a]);
                    }
                    _ => {
                        p.back = None;
                        let mut px_ = std::mem::take(&mut p.frame.bgra);
                        p.store.frame_into(cur, &mut px_, p.decoded);
                        p.frame.bgra = px_;
                    }
                }
            }
            p.decoded = Some(cur);
            let pitch = p.w * 4;
            crate::insight_scope!("gif_upload");
            if let Err(e) = unsafe {
                p.bitmap
                    .CopyFromMemory(None, p.frame.bgra.as_ptr().cast(), pitch)
            } {
                eprintln!("gifpin: frame upload failed: {e}");
            }
            p.shown = cur;
            let lay = tray_layout(&p);
            let items = strip_items(&p.tl, p.s);
            // ストリップの矩形ドラッグ中と、帯の下の段のつまみのドラッグ中は追わない (跳ぶ)。
            if !p.marquee.is_some_and(|m| m.moved) && !matches!(p.drag, Some(Drag::Scroll(_))) {
                let w = lay.view.right - lay.view.left;
                // 上の段のシーク (押下とドラッグ) は今のフレームを窓の中央に固定する。ほかは見えていれば動かさない。
                p.scroll = if p.drag == Some(Drag::Seek) {
                    center_scroll(&items, w, cur).unwrap_or(p.scroll)
                } else {
                    follow(&items, p.scroll, w, cur, p.playing.is_some())
                };
            }
        }
        // 再生中は毎刻みなので書かない。番号は 1 始まり。
        if p.playing.is_none() {
            let (n, d) = p.tl.status();
            let one = |r: (usize, usize)| (r.0 + 1, r.1 + 1);
            eprintln!(
                "gifpin: frame {} sel {:?} trim {:?} deleted {} status {n} / {:.2} s, export {}",
                cur + 1,
                p.tl.sel.map(one),
                one(p.tl.trim),
                p.tl.deleted.len(),
                d.as_secs_f64(),
                p.tl.export_span().0
            );
        }
    }
    // ツールバーはフレームに依らないので描き直さない (シークと再生の毎コマで要らない)。
    let p = pin.borrow();
    paint_pin(&p);
    paint_tray(&p);
}

fn set_playing(pin: &Rc<RefCell<GifPin>>, on: bool) {
    let mut p = pin.borrow_mut();
    if on == p.playing.is_some() {
        return;
    }
    unsafe {
        if on {
            // 既定の 15.6 ms 刻みでは 33 ms のフレームが 31 / 47 ms に揺れる。再生中だけ 1 ms にする。
            timeBeginPeriod(1);
            p.playing = Some((Instant::now(), Duration::ZERO));
            schedule_play(p.hwnd, p.tl.own(p.tl.cur));
        } else {
            p.playing = None;
            let _ = KillTimer(Some(p.hwnd), TIMER_PLAY);
            timeEndPeriod(1);
        }
    }
    eprintln!("gifpin: play {on} at frame {}", p.tl.cur + 1);
}

fn play_tick(pin: &Rc<RefCell<GifPin>>) {
    {
        let mut p = pin.borrow_mut();
        let Some((last, carry)) = p.playing else {
            return;
        };
        let now = Instant::now();
        let (cur, left) = p.tl.advance(p.tl.cur, now - last + carry);
        p.tl.cur = cur;
        p.playing = Some((now, left));
        schedule_play(p.hwnd, p.tl.own(cur).saturating_sub(left));
    }
    update(pin);
}

/// 次のフレームの切り替わりに合わせて再生の刻みを張り直す (毎刻み)。
fn schedule_play(hwnd: HWND, wait: Duration) {
    let ms = wait.as_micros().div_ceil(1000).clamp(1, u32::MAX as u128) as u32;
    unsafe {
        SetTimer(Some(hwnd), TIMER_PLAY, ms, None);
    }
}

/// ホバーで出す。もう出ていれば何もしない。
fn show_bar(pin: &Rc<RefCell<GifPin>>) {
    let mut p = pin.borrow_mut();
    if p.bar.visible() {
        return;
    }
    p.bar_hover = None;
    p.bar_pressed = None;
    p.anims.borrow_mut().clear();
    place_bar(&mut p);
    paint_bar(&p);
}

/// ツールバーを残すか: カーソルがピンの窓 (画像と影の余白)・トレイ・ツールバーにある、またはドラッグ中。
fn bar_tick(pin: &Rc<RefCell<GifPin>>) {
    let pt = cursor();
    let Ok(mut p) = pin.try_borrow_mut() else {
        return;
    };
    if !p.bar.visible() {
        // ponytail: 最前面オフでは、ほかのアプリの窓がその場所を覆っていても出る。
        // このアプリの別のピンやツールバーが出ていれば出さない (重なった 2 つが交互に上へ出て震えた)。
        let hit = p.drag.is_none()
            && !p.modal
            && contains(&bar_rect(&p), pt)
            && !ui::ours_above(p.hwnd);
        drop(p);
        if hit {
            show_bar(pin);
        }
        return;
    }
    let keep = p.drag.is_some()
        || p.modal
        || p.edit.is_some()
        || p.bar_pressed.is_some()
        || [pin_window_rect(p.img, p.s), p.bar_rect]
            .iter()
            .any(|r| contains(r, pt))
        || (p.timeline && contains(&p.tray_rect, pt))
        || ((p.pop_open || p.ann_pop.is_some()) && contains(&p.pop_rect, pt))
        || ((p.list_open.is_some() || p.zoom_open) && contains(&p.list_rect, pt));
    if !keep {
        p.bar_hover = None;
        p.bar.hide();
        set_zoom_list(&mut p, false);
        if p.pop_open {
            set_pop(&mut p, false);
        }
        if p.tray_hover.is_none() {
            p.tip.hide();
        }
        unsafe {
            let _ = KillTimer(Some(p.bar.hwnd), TIMER_TIP);
        }
    }
}

fn set_topmost(pin: &Rc<RefCell<GifPin>>, on: bool) {
    let mut p = pin.borrow_mut();
    p.topmost = on;
    unsafe {
        let _ = SetWindowPos(
            p.hwnd,
            Some(if on { HWND_TOPMOST } else { HWND_NOTOPMOST }),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    // トレイ・ツールバー・ツールチップも一緒に下ろす (残すと画像から離れて浮く)。
    for h in [&p.tray, &p.bar, &p.tip, &p.pop, &p.list] {
        h.set_topmost(on);
    }
    eprintln!("gifpin: topmost {on}");
}

fn close(pin: &Rc<RefCell<GifPin>>) {
    let hwnd = pin.borrow().hwnd;
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

fn bar_action(pin: &Rc<RefCell<GifPin>>, i: usize) {
    if pin.borrow().edit.is_some() {
        ann_action(pin, i);
        return;
    }
    match i {
        I_COPY => copy(pin),
        I_ANNOTATE => enter_edit(pin),
        I_FRAME => copy_frame(pin),
        I_SAVE => save(pin),
        I_OPT => {
            let mut p = pin.borrow_mut();
            let open = !p.pop_open;
            set_pop(&mut p, open);
        }
        I_ZOOM => {
            let mut p = pin.borrow_mut();
            let open = !p.zoom_open;
            set_zoom_list(&mut p, open);
        }
        I_TOPMOST => {
            let on = !pin.borrow().topmost;
            set_topmost(pin, on);
        }
        I_CLOSE => close(pin),
        _ => {}
    }
}

fn show_bar_tip(pin: &Rc<RefCell<GifPin>>) {
    let pal: &Palette = ui::palette();
    let mut p = pin.borrow_mut();
    let Some(i) = p
        .bar_hover
        .filter(|_| p.bar_pressed.is_none() && p.ann_pop.is_none())
    else {
        return;
    };
    let (name, key) = match (&p.edit, i) {
        (Some(e), i) => annot_edit::tip(i, e.tool),
        (None, i) => bar_tip(i),
    };
    if name.is_empty() {
        return;
    }
    let s = p.s;
    let tip = Tooltip::new(name, key, s);
    let (layout, _) = toolbar_layout(&items(&p), s);
    let (x, bw) = layout[i];
    let top = p.bar_rect.top + px(4.0, s);
    let target = RECT {
        left: p.bar_rect.left + x,
        top,
        right: p.bar_rect.left + x + bw,
        bottom: top + px(32.0, s),
    };
    let at = place_tooltip(target, tip.w, tip.h, px(8.0, s), work_area(p.bar_rect));
    p.tip.show_at(RECT {
        left: at.x,
        top: at.y,
        right: at.x + tip.w,
        bottom: at.y + tip.h,
    });
    p.tip.paint(pal, |ctx| tip.draw(ctx, pal, s));
}

/// 通常のツールバーのツールチップの (名前, キー)。
fn bar_tip(i: usize) -> (&'static str, &'static str) {
    let t = crate::strings();
    match i {
        I_COPY => (t.copy, "Ctrl+C"),
        I_FRAME => (t.copy_frame, ""),
        I_SAVE => (t.save, "Ctrl+S"),
        I_ANNOTATE => (t.annotate, ""),
        I_OPT => (t.export_size, ""),
        I_ZOOM => (t.zoom, ""),
        I_TOPMOST => (t.topmost, ""),
        I_CLOSE => (t.close, "Esc"),
        _ => ("", ""),
    }
}

/// Delete とトレイの削除のボタン: 注釈を選んでいればその注釈を、選んでいなければ範囲のフレームを消す。
fn delete_key(pin: &Rc<RefCell<GifPin>>) {
    let annot = pin.borrow().edit.as_ref().is_some_and(Editor::any_sel);
    if annot {
        annot_edit::delete_selected(&(pin.clone() as HostRef));
    } else {
        delete(pin);
    }
}

fn delete(pin: &Rc<RefCell<GifPin>>) {
    set_playing(pin, false);
    let done = {
        let mut p = pin.borrow_mut();
        let done = p.tl.delete();
        if done {
            p.ann.get_mut().hist.frames();
        }
        done
    };
    let p = pin.borrow();
    eprintln!(
        "gifpin: delete {done}: {} deleted, export {} frames",
        p.tl.deleted.len(),
        p.tl.export_span().0
    );
    drop(p);
    // 今のフレームが変わらなくても、ストリップの並びが変わる。
    pin.borrow_mut().shown = usize::MAX;
    update(pin);
}

/// 別スレッドのエンコードの結果。
struct Encoded {
    /// `true` はコピー (一時 .gif)、`false` は保存。
    copy: bool,
    path: PathBuf,
    bytes: Vec<u8>,
    written: std::io::Result<()>,
    /// サイズ上限まで下げ切っても超えた。
    over: bool,
    /// 上限に合わせて下げた fps と倍率 (下げる段を通ったときだけ)。
    reduced: Option<(u32, u32)>,
    /// 書き出したときの上限 (MB)。
    limit_mb: u32,
}

/// 進捗 (何回目, 書いたフレーム数, そのエンコードのフレーム数) を 1 つの数に詰める (UI スレッドが 1 回で読む)。
fn pack_progress(pass: u32, done: usize, total: usize) -> u64 {
    (u64::from(pass) << 48) | ((done as u64 & 0xFF_FFFF) << 24) | (total as u64 & 0xFF_FFFF)
}

fn unpack_progress(v: u64) -> (u32, usize, usize) {
    (
        (v >> 48) as u32,
        ((v >> 24) & 0xFF_FFFF) as usize,
        (v & 0xFF_FFFF) as usize,
    )
}

/// GIF を別スレッドでエンコードして `path` へ書く。終わったら窓へ `WM_GIF_DONE` を投函する。
/// 窓が先に閉じたら投函は捨てられ、書いたファイルは残る。
fn start_export(pin: &Rc<RefCell<GifPin>>, path: PathBuf, copy: bool) {
    let (tx, rx) = mpsc::channel();
    let progress = Arc::new(AtomicU64::new(pack_progress(1, 0, 0)));
    let (store, opts, limit, limit_mb, (w, h), hwnd) = {
        let p = pin.borrow();
        let base = p.tl.options(None);
        // 注釈の層は D2D (`ui::gfx()`) で描くので、ここ (UI スレッド) で作って渡す。
        let annots = annot::segments(&p.ann.borrow().spans, base.range.clone())
            .into_iter()
            .map(|(frames, on)| {
                let items: Vec<_> = on.iter().map(|&k| p.doc.items[k].clone()).collect();
                AnnotSeg {
                    frames,
                    layer: annot::layer(p.w, p.h, &items),
                    mosaics: annot::mosaic_rects(&items, p.w, p.h),
                }
            })
            .collect();
        let opts = GifOptions {
            scale_pct: p.scale,
            annots,
            ..base
        };
        // 上限は 10 進の MB (trim-shot のサイズ指定と同じ)。
        let limit = (p.limit_mb > 0).then(|| p.limit_mb as usize * 1_000_000);
        // HWND は Send でないので数で渡す。
        (
            p.store.clone(),
            opts,
            limit,
            p.limit_mb,
            (p.w, p.h),
            p.hwnd.0 as isize,
        )
    };
    let prog = progress.clone();
    std::thread::spawn(move || {
        let started = Instant::now();
        let (range, deleted) = (opts.range.clone(), opts.deleted.len());
        let fit = crate::clip::encode_gif_within(&store, opts, limit, &mut |pass, d, n| {
            prog.store(pack_progress(pass, d, n), Ordering::Relaxed)
        });
        let out = scaled_size(w, h, fit.scale_pct);
        eprintln!(
            "gifpin: encoded range {}..{} (1-based {}–{}), {} deleted, decimate {:?}, scale {}% {}x{}, limit {:?}{}: {} bytes in {:.0} ms",
            range.start,
            range.end,
            range.start + 1,
            range.end,
            deleted,
            fit.decimate_fps,
            fit.scale_pct,
            out.0,
            out.1,
            limit,
            if fit.over { " (over)" } else { "" },
            fit.bytes.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );
        let (bytes, over) = (fit.bytes, fit.over);
        let written = std::fs::write(&path, &bytes);
        let failed = written.is_err();
        let _ = tx.send(Encoded {
            copy,
            path: path.clone(),
            bytes,
            written,
            over,
            reduced: fit.reduced_fps.map(|f| (f, fit.scale_pct)),
            limit_mb,
        });
        let posted = unsafe {
            PostMessageW(
                Some(HWND(hwnd as *mut _)),
                WM_GIF_DONE,
                WPARAM(0),
                LPARAM(0),
            )
        };
        if posted.is_err() {
            // ピンが先に閉じた: 帯の代わりにトレイのバルーンで知らせる。コピーはクリップボードの持ち主の窓が
            // 無いので載せられない (一時 .gif も消す)。ponytail: 投函の後、処理の前に閉じた分は知らせない。
            let t = crate::strings();
            if copy {
                let _ = std::fs::remove_file(&path);
                crate::notify(crate::tray_hwnd(), t.copy_failed);
            } else if failed {
                crate::notify(crate::tray_hwnd(), t.save_failed);
            } else if over {
                crate::notify(crate::tray_hwnd(), t.export_over_limit);
            }
        }
    });
    let mut p = pin.borrow_mut();
    p.encoding = Some(rx);
    p.progress = progress;
    p.export_copy = copy;
    unsafe {
        SetTimer(Some(p.hwnd), TIMER_BUSY, BUSY_LOOK_MS, None);
    }
}

/// エンコードが長引いた: コピーと保存を無効の見た目に描き直す (Ctrl+C / Ctrl+S の経路でも)。
fn busy_tick(pin: &Rc<RefCell<GifPin>>) {
    let mut p = pin.borrow_mut();
    unsafe {
        let _ = KillTimer(Some(p.hwnd), TIMER_BUSY);
    }
    p.busy_look = p.encoding.is_some();
    if p.busy_look && p.bar.visible() {
        paint_bar(&p);
    }
    if p.busy_look {
        let (pass, done, total) = unpack_progress(p.progress.load(Ordering::Relaxed));
        let op = if p.export_copy { "copy" } else { "save" };
        drop(p);
        show_status(pin, Band::progress(op, progress_label(pass), done, total));
    }
}

fn progress_label(pass: u32) -> String {
    let t = crate::strings();
    if pass <= 1 {
        t.band_encoding.to_string()
    } else {
        t.band_reencoding.replace("{n}", &pass.to_string())
    }
}

/// 帯の中身を替えて刻みを回す。
fn show_status(pin: &Rc<RefCell<GifPin>>, band: Band) {
    let mut p = pin.borrow_mut();
    eprintln!(
        "gifpin: status {:?} {} {:?} {:?}",
        band.kind, band.label, band.size, band.sub
    );
    p.status.show(band);
    unsafe {
        SetTimer(Some(p.hwnd), TIMER_STATUS, STATUS_TICK_MS, None);
    }
    p.status.tick();
    paint_pin(&p);
}

/// 帯の刻み: 進捗の数を読み、出入りと残す時間を進める。帯が消えたら止める。
fn status_tick(pin: &Rc<RefCell<GifPin>>) {
    let mut p = pin.borrow_mut();
    if p.encoding.is_some() {
        let (pass, done, total) = unpack_progress(p.progress.load(Ordering::Relaxed));
        p.status.set_progress(progress_label(pass), done, total);
    }
    let repaint = p.status.tick();
    if !p.status.active() {
        unsafe {
            let _ = KillTimer(Some(p.hwnd), TIMER_STATUS);
        }
    }
    if repaint {
        paint_pin(&p);
    }
}

/// 書き出しの結果の帯: 「コピーしました · 0.9 MB」。下げたときと超えたときは 2 行目にその値を出す。
fn result_band(label: &str, e: &Encoded) -> Band {
    let t = crate::strings();
    let values = |text: &str, (fps, scale): (u32, u32)| {
        text.replace("{fps}", &fps.to_string())
            .replace("{scale}", &scale.to_string())
            .replace("{mb}", &e.limit_mb.to_string())
    };
    let sub = e
        .reduced
        .map(|v| values(if e.over { t.band_over } else { t.band_lowered }, v));
    let kind = if e.over { Kind::Warn } else { Kind::Done };
    Band::result(
        kind,
        label,
        Some(status::size_text(e.bytes.len(), e.over)),
        sub,
    )
}

/// エンコードが終わった: コピーならクリップボードへ載せる (所有者の窓が要るので UI スレッドで)。
fn finish_export(pin: &Rc<RefCell<GifPin>>) {
    let Some(e) = pin
        .borrow_mut()
        .encoding
        .take()
        .and_then(|rx| rx.try_recv().ok())
    else {
        return;
    };
    let hwnd = pin.borrow().hwnd;
    let t = crate::strings();
    let failed = |label| Band::result(Kind::Warn, label, None, None);
    let band = match &e.written {
        Err(err) => {
            eprintln!("gifpin: write {} failed: {err}", e.path.display());
            failed(if e.copy {
                t.band_copy_failed
            } else {
                t.band_save_failed
            })
        }
        Ok(()) if e.copy => {
            pin.borrow_mut().temp.push(e.path.clone());
            match crate::export::copy_gif(hwnd, &e.path, &e.bytes) {
                Ok(()) => {
                    eprintln!("gifpin: copied {}", e.path.display());
                    result_band(t.band_copied, &e)
                }
                Err(err) => {
                    eprintln!("gifpin: copy failed: {err}");
                    failed(t.band_copy_failed)
                }
            }
        }
        Ok(()) => {
            eprintln!("gifpin: saved {}", e.path.display());
            result_band(t.band_saved, &e)
        }
    };
    {
        let mut p = pin.borrow_mut();
        unsafe {
            let _ = KillTimer(Some(p.hwnd), TIMER_BUSY);
        }
        if std::mem::take(&mut p.busy_look) && p.bar.visible() {
            paint_bar(&p);
        }
    }
    show_status(pin, band);
}

/// エンコード中か保存ダイアログが開いているなら、コピーと保存は何もしない。
fn busy(pin: &Rc<RefCell<GifPin>>) -> bool {
    let busy = {
        let p = pin.borrow();
        p.encoding.is_some() || p.modal
    };
    if busy {
        eprintln!("gifpin: busy, export ignored");
    }
    busy
}

fn temp_path() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(crate::PRODUCT_NAME);
    std::fs::create_dir_all(&dir).ok()?;
    let name = crate::export::default_name("gif");
    let stem = name.trim_end_matches(".gif").to_string();
    (1..1000)
        .map(|k| {
            if k == 1 {
                dir.join(&name)
            } else {
                dir.join(format!("{stem} ({k}).gif"))
            }
        })
        .find(|p| !p.exists())
}

fn copy(pin: &Rc<RefCell<GifPin>>) {
    if busy(pin) {
        return;
    }
    let Some(path) = temp_path() else {
        eprintln!("gifpin: no temp folder");
        let t = crate::strings();
        show_status(
            pin,
            Band::result(Kind::Warn, t.band_copy_failed, None, None),
        );
        return;
    };
    start_export(pin, path, true);
}

fn save(pin: &Rc<RefCell<GifPin>>) {
    if busy(pin) {
        return;
    }
    set_playing(pin, false);
    let hwnd = pin.borrow().hwnd;
    let dir = crate::settings().save_dir.clone();
    // 先は先に UI スレッドで聞く (モーダル)。取り消しならエンコードしない。
    // ダイアログが無効にするのはピンの窓だけで、HUD の窓は押せるので `modal` で塞ぐ。
    pin.borrow_mut().modal = true;
    let asked = crate::export::ask_path(hwnd, "gif", dir.as_deref());
    pin.borrow_mut().modal = false;
    match asked {
        Ok(Some(path)) => start_export(pin, path, false),
        Ok(None) => {}
        Err(e) => {
            eprintln!("gifpin: save failed: {e}");
            let t = crate::strings();
            show_status(
                pin,
                Band::result(Kind::Warn, t.band_save_failed, None, None),
            );
        }
    }
}

fn copy_frame(pin: &Rc<RefCell<GifPin>>) {
    crate::insight_scope!("gif_copy_frame");
    let copied = {
        let p = pin.borrow();
        // `frame` は `update` で今のフレームにしてある。注釈はそのフレームで出ている物だけ合成する。
        let image = annot::composite_frame(
            &p.frame,
            &p.doc.items,
            &p.ann.borrow().spans,
            &p.tl,
            p.tl.cur,
        );
        match crate::export::copy(p.hwnd, &image) {
            Ok(()) => {
                eprintln!(
                    "gifpin: copied frame {} ({:.2} s)",
                    p.tl.cur + 1,
                    p.tl.ts[p.tl.cur].saturating_sub(p.tl.ts[0]).as_secs_f64()
                );
                true
            }
            Err(e) => {
                eprintln!("gifpin: copy frame failed: {e}");
                false
            }
        }
    };
    let t = crate::strings();
    let band = if copied {
        Band::result(Kind::Done, t.band_copied, None, None)
    } else {
        Band::result(Kind::Warn, t.band_copy_failed, None, None)
    };
    show_status(pin, band);
}

fn set_zoom(pin: &Rc<RefCell<GifPin>>, zoom: f32, around: POINT) {
    // 入力中の文字の欄は画像の位置に重ねてあるので、先に確定する。
    annot_edit::finish_text(&(pin.clone() as HostRef), true);
    {
        let mut p = pin.borrow_mut();
        let zoom = zoom.clamp(0.1, 8.0);
        let zoom = if (zoom - 1.0).abs() < 0.01 { 1.0 } else { zoom };
        let k = zoom / p.zoom;
        let w = (p.w as f32 * zoom).round().max(1.0) as i32;
        let h = (p.h as f32 * zoom).round().max(1.0) as i32;
        let left = around.x - ((around.x - p.img.left) as f32 * k).round() as i32;
        let top = around.y - ((around.y - p.img.top) as f32 * k).round() as i32;
        p.zoom = zoom;
        p.img = RECT {
            left,
            top,
            right: left + w,
            bottom: top + h,
        };
    }
    apply_rect(pin);
}

fn scroll_strip(pin: &Rc<RefCell<GifPin>>, delta: i16) {
    {
        let mut p = pin.borrow_mut();
        let lay = tray_layout(&p);
        let items = strip_items(&p.tl, p.s);
        let max = (strip_width(&items) - (lay.view.right - lay.view.left)).max(0);
        let step = px(60.0, p.s) * 3;
        p.scroll = (p.scroll - delta as i32 * step / WHEEL_DELTA as i32).clamp(0, max);
    }
    paint_all(pin);
}

/// 帯の上の押下: 再生のボタン、トリムの括弧、全体バー (Shift で範囲)。帯の外なら `false`。
fn band_down(pin: &Rc<RefCell<GifPin>>, pt: POINT) -> bool {
    let (band, g, s) = {
        let p = pin.borrow();
        let (band, g) = band_of(&p);
        (band, g, p.s)
    };
    if !pin.borrow().timeline || !contains(&band, pt) {
        return false;
    }
    let (x, y) = (pt.x - band.left, pt.y - band.top);
    if contains(&g.play, POINT { x, y }) {
        let on = pin.borrow().playing.is_none();
        set_playing(pin, on);
        paint_all(pin);
        return true;
    }
    let slack = px(6.0, s);
    if x < g.bar_x - slack || x > g.bar_x + g.bar_w + slack {
        return true;
    }
    if y >= g.split {
        // 下の段はストリップのスクロールだけ。再生位置は動かさず、再生も止めない。
        // つまみの上はつかんだ位置のずれを持ってドラッグ、レールの空いた所は押した位置のフレームを中央に送り、
        // そのままつまみの中心を押した位置としてドラッグを続ける。
        {
            let mut p = pin.borrow_mut();
            let off = match rail_thumb(&p, &g) {
                Some((l, r)) if (x as f32) >= l && (x as f32) <= r => {
                    x - ((l + r) / 2.0).round() as i32
                }
                _ => {
                    rail_scroll_to(&mut p, x);
                    0
                }
            };
            p.drag = Some(Drag::Scroll(off));
            p.tip.hide();
            eprintln!("gifpin: strip scroll {} (rail)", p.scroll);
        }
        unsafe {
            SetCapture(pin.borrow().hwnd);
        }
        paint_all(pin);
        return true;
    }
    // 再生はどのドラッグでも止める。離しても止めたまま (再び再生するのは Space)。
    let drag = {
        let p = pin.borrow();
        match bracket_hit(x, &p.tl, &g, s) {
            Some(start) => Drag::Trim(start),
            None if shift() => Drag::Select,
            None => Drag::Seek,
        }
    };
    set_playing(pin, false);
    {
        let mut p = pin.borrow_mut();
        let n = p.tl.len();
        p.drag = Some(drag);
        apply_band_drag(&mut p, drag, frame_at(x, n, &g));
    }
    unsafe {
        SetCapture(pin.borrow().hwnd);
    }
    update(pin);
    true
}

fn apply_band_drag(p: &mut GifPin, drag: Drag, i: usize) {
    match drag {
        Drag::Seek => p.tl.seek(i),
        Drag::Select => p.tl.select_to(i),
        Drag::Trim(true) => p.tl.set_trim_start(i),
        Drag::Trim(false) => p.tl.set_trim_end(i),
        Drag::Move(_) | Drag::Scroll(_) => {}
    }
}

/// 下の段のつまみ (`thumb_x`、帯の左から)。ストリップに何も見えていなければ無い。
fn rail_thumb(p: &GifPin, g: &BandGeo) -> Option<(f32, f32)> {
    visible(p).map(|(a, b)| thumb_x((a as f32, b as f32), p.tl.len(), g, p.s))
}

/// 帯の下の段の当たり (画面の点): 段の中なら `Some(つまみの上か)`。横は全体バーの左右 6 まで。
fn rail_hit(p: &GifPin, pt: POINT) -> Option<bool> {
    if !p.timeline {
        return None;
    }
    let (band, g) = band_of(p);
    if !contains(&band, pt) {
        return None;
    }
    let (x, y) = (pt.x - band.left, pt.y - band.top);
    let slack = px(6.0, p.s);
    if y < g.split || x < g.bar_x - slack || x > g.bar_x + g.bar_w + slack {
        return None;
    }
    Some(rail_thumb(p, &g).is_some_and(|(l, r)| (x as f32) >= l && (x as f32) <= r))
}

/// 下の段: 帯の左からの x が指すフレーム (削除したフレームはそれを含む `Gap`) を、ストリップの窓の中央に送る。
fn rail_scroll_to(p: &mut GifPin, x: i32) {
    let (_, g) = band_of(p);
    let lay = tray_layout(p);
    let items = strip_items(&p.tl, p.s);
    let f = frame_at(x, p.tl.len(), &g);
    if let Some(to) = center_scroll(&items, lay.view.right - lay.view.left, f) {
        p.scroll = to;
    }
}

/// 下の段をホバー中かドラッグ中: レールは `track`、つまみは高さ 6 の `on-fill`。
fn rail_hot(p: &GifPin) -> bool {
    p.rail_hover || matches!(p.drag, Some(Drag::Scroll(_)))
}

/// 下の段のホバーが変わったら描き直し、入ったらツールチップの時計 (`hwnd` の `TIMER_TIP`) を張る。
/// 時計は止めない: トレイの窓ではボタンのツールチップと同じ時計で、鳴ったときに `rail_hover` を見て分ける。
fn set_rail_hover(pin: &Rc<RefCell<GifPin>>, hwnd: HWND, hover: bool) {
    if std::mem::replace(&mut pin.borrow_mut().rail_hover, hover) == hover {
        return;
    }
    pin.borrow().tip.hide();
    if hover {
        unsafe {
            SetTimer(Some(hwnd), TIMER_TIP, 500, None);
        }
    }
    paint_all(pin);
}

/// 下の段のツールチップ「ストリップの表示範囲」: 帯の上端の 6 上に、つまみの中心に合わせて出す。
fn show_rail_tip(pin: &Rc<RefCell<GifPin>>) {
    let pal: &Palette = ui::palette();
    let mut p = pin.borrow_mut();
    if !p.rail_hover || p.drag.is_some() {
        return;
    }
    let s = p.s;
    let (band, g) = band_of(&p);
    let tip = Tooltip::new(crate::strings().strip_range, "", s);
    let cx = band.left
        + rail_thumb(&p, &g).map_or(g.bar_x + g.bar_w / 2, |(l, r)| {
            ((l + r) / 2.0).round() as i32
        });
    let work = work_area(band);
    let x = (cx - tip.w / 2).clamp(work.left, (work.right - tip.w).max(work.left));
    let y = band.top - px(6.0, s) - tip.h;
    p.tip.show_at(RECT {
        left: x,
        top: y,
        right: x + tip.w,
        bottom: y + tip.h,
    });
    p.tip.paint(pal, |ctx| tip.draw(ctx, pal, s));
}

fn key_down(pin: &Rc<RefCell<GifPin>>, vk: u16) {
    let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
    let editing = pin.borrow().edit.is_some();
    match (ctrl, vk) {
        (true, 0x43) => copy(pin),
        // Ctrl+Z 元に戻す、Ctrl+Y / Ctrl+Shift+Z やり直す (フレームの削除と注釈の 1 本の列)。
        (true, 0x5A) | (true, 0x59) => history(pin, vk == 0x59 || shift()),
        (true, 0x53) => save(pin),
        (true, 0x30) | (true, 0x60) => {
            let img = pin.borrow().img;
            set_zoom(
                pin,
                1.0,
                POINT {
                    x: img.left,
                    y: img.top,
                },
            );
        }
        (false, k) if k == VK_LEFT.0 || k == VK_RIGHT.0 => {
            // コマ送りは再生を止める (再び再生するのは Space)。
            set_playing(pin, false);
            pin.borrow_mut().tl.step(k == VK_RIGHT.0);
            update(pin);
        }
        (false, k) if k == VK_SPACE.0 => {
            let on = pin.borrow().playing.is_none();
            set_playing(pin, on);
            paint_all(pin);
        }
        (false, k) if k == VK_DELETE.0 => delete_key(pin),
        // 注釈モードの Esc は選択を外し、何も選んでいなければ注釈モードを終える (ピンは閉じない)。
        (false, k) if k == VK_ESCAPE.0 && editing => {
            if annot_edit::deselect(&(pin.clone() as HostRef)) {
                paint_tray(&pin.borrow());
            } else {
                leave_edit(pin);
            }
        }
        (false, k) if k == VK_ESCAPE.0 => close(pin),
        _ => {}
    }
}

/// 元に戻す・やり直す: 印の列 (`annot::History`) から 1 手。フレームの削除も注釈も、この列だけを通す。
fn history(pin: &Rc<RefCell<GifPin>>, redo: bool) {
    set_playing(pin, false);
    if pin.borrow().edit.is_some() {
        // 入力中の文字を確定し、選択を外し、ドラッグを終える (どれも `Doc` に手を積みうる)。
        annot_edit::settle(&(pin.clone() as HostRef));
    }
    let done = {
        let mut guard = pin.borrow_mut();
        sync_ann(&guard);
        let p = &mut *guard;
        let a = p.ann.get_mut();
        let done = a.hist.undo_redo(redo, &mut p.tl, &mut p.doc, &mut a.spans);
        // 戻した手は印の列が回したもの: 次の `sync_ann` で新しい手と読まない。
        a.snap = p.doc.items.clone();
        a.depth = p.doc.depth();
        done
    };
    eprintln!(
        "gifpin: {} {done}, {} items",
        if redo { "redo" } else { "undo" },
        pin.borrow().doc.items.len()
    );
    if done {
        pin.borrow_mut().shown = usize::MAX;
        update(pin);
    }
    let p = pin.borrow();
    if p.bar.visible() {
        paint_bar(&p);
    }
}

/// エディタが `Doc` に積んだ手に、区間と undo の印を合わせる (足した注釈の区間は全体、消した注釈の区間は消す)。
/// 手の数は `doc.depth()` で知り、どの注釈かは前の `doc.items` との違いで知る (`Doc::add` はいつも末尾に足す)。
fn sync_ann(p: &GifPin) {
    let Ok(mut guard) = p.ann.try_borrow_mut() else {
        return;
    };
    let a = &mut *guard;
    let depth = p.doc.depth();
    if depth > a.depth {
        let before = a.spans.clone();
        let (old, new) = (a.snap.len(), p.doc.items.len());
        if new > old {
            a.spans.push((0, p.tl.len() - 1));
        } else if new < old {
            // 消した注釈 (矩形選択なら複数) の区間を消す: 残った注釈は前の並びの部分列なので、前から照らし合わせる。
            // ponytail: 同じ注釈が隣り合っていると、消した方でなく隣の区間を消すことがある。困ったら Doc に消した添字を聞く。
            let mut rest = p.doc.items.iter().peekable();
            let mut k = 0;
            a.spans.retain(|_| {
                let keep = a.snap.get(k).is_none_or(|x| rest.next_if(|y| *y == x).is_some());
                k += 1;
                keep
            });
        }
        for _ in a.depth..depth {
            a.hist.doc(before.clone());
        }
    }
    a.depth = depth;
    if a.snap != p.doc.items {
        a.snap = p.doc.items.clone();
    }
}

/// 注釈 `i` の見え方: 今のフレームが実効の区間 (削除とトリムを除く) にあれば出す。区間外は、注釈モードで
/// トグルがオンのときだけ灰色。実効の区間が空の注釈 (区間が全部消えた) はいつも出さない。
fn vis(p: &GifPin, i: usize) -> Vis {
    let Ok(a) = p.ann.try_borrow() else {
        return Vis::Shown;
    };
    // 合わせる前 (エディタが足した直後の描画) の注釈は区間が全体。
    let Some(&(s0, s1)) = a.spans.get(i) else {
        return Vis::Shown;
    };
    let cur = p.tl.cur;
    if s0 <= cur && cur <= s1 && p.tl.kept(cur) && p.tl.in_trim(cur) {
        Vis::Shown
    } else if p.edit.is_some() && p.ghosts && p.tl.effective((s0, s1)).next().is_some() {
        Vis::Ghost
    } else {
        Vis::Hidden
    }
}

/// 注釈を選んでいる間のストリップの範囲: ドラッグか Shift+クリックで作った範囲 (`make`) を、選んだ注釈の
/// 区間にする (1 手)。範囲は消す (ただのクリックはシークだけ)。注釈を選んでいなければ何もしない。
fn commit_span(p: &mut GifPin, make: bool) {
    let Some(k) = p.edit.as_ref().and_then(Editor::sel) else {
        return;
    };
    let sel = p.tl.sel.take();
    let (Some(r), true) = (sel, make) else {
        return;
    };
    let a = p.ann.get_mut();
    if a.spans.get(k).is_some_and(|&old| old != r) {
        let before = a.spans.clone();
        a.spans[k] = r;
        a.hist.span(before);
        eprintln!("gifpin: span {k} -> {}–{}", r.0 + 1, r.1 + 1);
    }
}

/// 注釈モードに入る: 再生を止め、ツールバーをエディタの並びに替え、帯をトレイの下の段に付ける (トレイは 108)。
fn enter_edit(pin: &Rc<RefCell<GifPin>>) {
    if pin.borrow().edit.is_some() {
        return;
    }
    set_playing(pin, false);
    set_timeline(pin, true);
    {
        let mut p = pin.borrow_mut();
        if p.pop_open {
            set_pop(&mut p, false);
        }
        set_zoom_list(&mut p, false);
        p.edit = Some(Editor::new());
        swap_bar(&mut p);
    }
    apply_rect(pin);
    let hwnd = pin.borrow().hwnd;
    unsafe {
        // Ctrl+Z・Delete・Esc をピンが受ける。
        let _ = SetForegroundWindow(hwnd);
    }
    let p = pin.borrow();
    let t = p.tray_rect;
    eprintln!(
        "gifpin: annotate on, bar {}x{}, tray {}x{}",
        p.bar_rect.right - p.bar_rect.left,
        p.bar_rect.bottom - p.bar_rect.top,
        t.right - t.left,
        t.bottom - t.top
    );
}

/// 「完了」: 入力中の文字を確定し、通常のツールバーに戻す。トレイは 72、帯は画像の下端に戻る。
fn leave_edit(pin: &Rc<RefCell<GifPin>>) {
    if pin.borrow().edit.is_none() {
        return;
    }
    annot_edit::finish_text(&(pin.clone() as HostRef), true);
    {
        let mut p = pin.borrow_mut();
        sync_ann(&p);
        p.edit = None;
        set_ann_pop(&mut p, None);
        swap_bar(&mut p);
    }
    apply_rect(pin);
    let p = pin.borrow();
    eprintln!(
        "gifpin: annotate off, {} items, tray {}",
        p.doc.items.len(),
        p.tray_rect.bottom - p.tray_rect.top
    );
}

/// ツールバーの並びが替わる: ホバー・押下・チップを外し、ボタンの遷移を捨てる (帯とトレイの鍵は残す)。
fn swap_bar(p: &mut GifPin) {
    p.bar_hover = None;
    p.bar_pressed = None;
    p.tip.hide();
    p.anims.borrow_mut().retain(|k| k.0 >= K_DELETE);
    p.painted = None;
}

/// 注釈のツールバーのボタン。
fn ann_action(pin: &Rc<RefCell<GifPin>>, i: usize) {
    use annot_edit::Action;
    let tool = pin.borrow().edit.as_ref().map_or(Tool::Arrow, |e| e.tool);
    let host = pin.clone() as HostRef;
    let action = annot_edit::action(i, tool);
    if let Some(Action::Tool(t)) = action {
        annot_edit::set_tool(&host, t);
        set_ann_pop(&mut pin.borrow_mut(), None);
        eprintln!("gifpin: tool {t:?}");
        return;
    }
    annot_edit::finish_text(&host, true);
    match action {
        Some(Action::Popup(c)) => {
            let mut p = pin.borrow_mut();
            let open = (p.ann_pop != Some(c)).then_some(c);
            set_ann_pop(&mut p, open);
        }
        Some(Action::Undo) => history(pin, false),
        Some(Action::Redo) => history(pin, true),
        Some(Action::Done) => leave_edit(pin),
        Some(Action::Tool(_)) | None => {}
    }
}

/// 色・線幅・文字の大きさを選んだ。文字の大きさは選んでも閉じない (静止画ピンと同じ)。
fn choose(pin: &Rc<RefCell<GifPin>>, c: Choice, idx: usize) {
    annot_edit::choose(&(pin.clone() as HostRef), c, idx);
    {
        let mut p = pin.borrow_mut();
        // `choose` は `toolbar_changed` を呼ばない: 属性の変更の手はここで合わせる。
        sync_ann(&p);
        if c != Choice::Size {
            set_ann_pop(&mut p, None);
        }
    }
    let p = pin.borrow();
    paint_pin(&p);
    paint_bar(&p);
    paint_pop(&p);
    eprintln!("gifpin: choose {c:?} {idx}");
}

// ---- アプリから ----

/// OS のテーマが変わった。トレイとポップアップを描き直す (画像の上の帯はいつもダーク、注釈モードのトレイの帯はトレイの色)。
pub fn theme_changed() {
    let dark = ui::apps_dark();
    let pins: Vec<_> = PINS.with(|p| p.borrow().clone());
    for pin in pins {
        {
            let p = pin.borrow();
            for a in [&p.tray, &p.tip, &p.bar, &p.pop, &p.list] {
                a.set_theme(dark);
            }
        }
        paint_all(&pin);
    }
}

/// アプリの終了: まだ開いているピンの一時 .gif を消す。
pub fn remove_temp_files() {
    PINS.with(|p| {
        for pin in p.borrow().iter() {
            for f in pin.borrow_mut().temp.drain(..) {
                let _ = std::fs::remove_file(&f);
            }
        }
    });
}

// ---- 窓 ----

fn tray_hit(p: &GifPin, pt: POINT) -> Option<TrayHot> {
    let lay = tray_layout(p);
    if contains(&lay.dock, pt) {
        Some(if p.edit.is_some() {
            TrayHot::Ghost
        } else {
            TrayHot::Dock
        })
    } else if contains(&lay.del, pt) {
        Some(TrayHot::Delete)
    } else if edge_mark(p).is_some_and(|(r, ..)| contains(&r, pt)) {
        Some(TrayHot::Edge)
    } else if contains(&lay.view, pt) {
        Some(TrayHot::Strip)
    } else {
        None
    }
}

/// ストリップの矩形ドラッグ。端の帯 (`EDGE_PX`) に入ると、入った深さに比例した速さでスクロールし、
/// 止まっていても `edge_pump` の刻みで続ける。今のコマを中央へ寄せる `follow` はドラッグ中は使わない (跳ぶ)。
/// マウスの移動でも刻みでも、前回からの経過時間ぶん進める (動かしている間は刻みが来ないことがある)。
fn marquee_move(pin: &Rc<RefCell<GifPin>>, hwnd: HWND, pt: POINT) {
    let edging = {
        let mut p = pin.borrow_mut();
        let lay = tray_layout(&p);
        let Some(mut m) = p.marquee else { return };
        let now = Instant::now();
        m.cur = pt;
        let start = lay.view.left + m.x0 - p.scroll;
        m.moved |= (pt.x - start).abs().max((pt.y - m.y0).abs()) >= px(4.0, p.s);
        p.marquee = Some(m);
        if !m.moved {
            return;
        }
        let items = strip_items(&p.tl, p.s);
        let edge = px(EDGE_PX, p.s);
        let over = if pt.x < lay.view.left + edge {
            pt.x - (lay.view.left + edge)
        } else if pt.x > lay.view.right - edge {
            pt.x - (lay.view.right - edge)
        } else {
            0
        };
        if over != 0 && !p.edge_timer {
            // 帯に入った: 最初の刻みの dt はここから測る。
            m.last = now;
            p.marquee = Some(m);
        }
        if over != 0 {
            let dt = now.duration_since(m.last).as_secs_f32().min(0.05);
            m.last = now;
            let max = (strip_width(&items) - (lay.view.right - lay.view.left)).max(0);
            let d = over.abs() as f32 / p.s;
            let o = (d - EDGE_PX).max(0.0);
            let v = (EDGE_SPEED * d + EDGE_ACCEL * d * d + EDGE_OUT * o * o).min(EDGE_MAX)
                * over.signum() as f32
                * p.s;
            m.acc += v * dt;
            let d = m.acc as i32;
            m.acc -= d as f32;
            p.scroll = (p.scroll + d).clamp(0, max);
            p.marquee = Some(m);
        }
        // 遠い側の辺にかかるフレームまで。窓の外は端のフレーム、隙間 (削除の印) の上は前のまま。
        // 窓の外で余白に当たったら、見えている端のフレーム (末尾までスクロールすると右端の 1 px は最後のサムネの右の余白)。
        let x = pt.x.clamp(lay.view.left, lay.view.right - 1) - lay.view.left;
        let vw = lay.view.right - lay.view.left;
        let hit = strip_hit(&items, p.scroll, x).or_else(|| {
            let (first, last) = visible_frames(&items, p.scroll, vw)?;
            if pt.x >= lay.view.right {
                Some(last)
            } else if pt.x < lay.view.left {
                Some(first)
            } else {
                None
            }
        });
        if let Some(i) = hit {
            p.tl.select_to(i);
        }
        over != 0
    };
    edge_timer(pin, hwnd, edging);
    update(pin);
    paint_tray(&pin.borrow());
}

/// 端のスクロールの刻みを回す・止める。刻みは画面の更新ごとに、別の糸がトレイの窓を `InvalidateRect` する。
/// 届く `WM_PAINT` は列が空のときだけ出て 1 つにまとまるので、投函と違って入力を押しのけない。
/// WM_TIMER は 15.6 ms 刻みで、279 Hz のモニタではシークバー (画面の更新ごと) より粗く見えた (2026-11-07 RTX 4070 の机)。
fn edge_timer(pin: &Rc<RefCell<GifPin>>, hwnd: HWND, on: bool) {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    let mut p = pin.borrow_mut();
    if p.edge_timer == on {
        return;
    }
    p.edge_timer = on;
    if !on {
        p.edge_gen.store(0, Ordering::Release);
        return;
    }
    let mine = NEXT.fetch_add(1, Ordering::Relaxed).max(1);
    p.edge_gen.store(mine, Ordering::Release);
    let (run, hwnd) = (p.edge_gen.clone(), hwnd.0 as usize);
    std::thread::spawn(move || {
        while run.load(Ordering::Acquire) == mine {
            unsafe {
                if DwmFlush().is_err() {
                    std::thread::sleep(Duration::from_millis(16));
                }
                // 窓が閉じていれば失敗する。
                if !InvalidateRect(Some(HWND(hwnd as _)), None, false).as_bool() {
                    break;
                }
            }
        }
    });
}

/// トレイの窓の `WM_PAINT`: 端のスクロールの 1 刻み。モーダルの間などで借りられなければ飛ばす。
fn edge_frame(pin: &Rc<RefCell<GifPin>>, hwnd: HWND) {
    let Ok(p) = pin.try_borrow() else { return };
    if !p.edge_timer {
        return;
    }
    let cur = p.marquee.map(|m| m.cur);
    drop(p);
    match cur {
        Some(pt) => marquee_move(pin, hwnd, pt),
        None => edge_timer(pin, hwnd, false),
    }
}

fn tray_msg(pin: &Rc<RefCell<GifPin>>, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) {
    if pin.borrow().modal && matches!(msg, WM_LBUTTONDOWN | WM_MOUSEMOVE) {
        return;
    }
    let pt = lparam_point(lparam);
    match msg {
        WM_MOUSEMOVE if pin.borrow().marquee.is_some() => marquee_move(pin, hwnd, pt),
        WM_MOUSEMOVE => {
            show_bar(pin);
            let changed = {
                let mut p = pin.borrow_mut();
                let hot = tray_hit(&p, pt);
                std::mem::replace(&mut p.tray_hover, hot) != hot
            };
            track_leave(hwnd);
            if changed {
                pin.borrow().tip.hide();
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_TIP);
                    if matches!(
                        pin.borrow().tray_hover,
                        Some(TrayHot::Delete | TrayHot::Dock | TrayHot::Ghost)
                    ) {
                        SetTimer(Some(hwnd), TIMER_TIP, 500, None);
                    }
                }
                paint_tray(&pin.borrow());
            }
            // 注釈モードの帯 (トレイのいちばん下の段) の再生のボタン。
            let play = {
                let p = pin.borrow();
                let (band, g) = band_of(&p);
                p.edit.is_some()
                    && contains(
                        &offset(g.play, band.left, band.top),
                        POINT {
                            x: pt.x + p.tray_rect.left,
                            y: pt.y + p.tray_rect.top,
                        },
                    )
            };
            if std::mem::replace(&mut pin.borrow_mut().play_hover, play) != play {
                paint_tray(&pin.borrow());
            }
            // 注釈モードの帯 (トレイのいちばん下の段) の下の段。
            let rail = {
                let p = pin.borrow();
                p.edit.is_some()
                    && rail_hit(
                        &p,
                        POINT {
                            x: pt.x + p.tray_rect.left,
                            y: pt.y + p.tray_rect.top,
                        },
                    )
                    .is_some()
            };
            set_rail_hover(pin, hwnd, rail);
        }
        0x02A3 /* WM_MOUSELEAVE */ => {
            {
                let mut p = pin.borrow_mut();
                p.tray_hover = None;
                p.rail_hover = false;
                p.play_hover = false;
            }
            pin.borrow().tip.hide();
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_TIP);
            }
            paint_tray(&pin.borrow());
        }
        WM_TIMER if wparam.0 == TIMER_TIP => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_TIP);
            }
            if pin.borrow().rail_hover {
                show_rail_tip(pin);
            } else {
                show_tray_tip(pin);
            }
        }
        WM_TIMER if wparam.0 == TIMER_ANIM => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_ANIM);
            }
            paint_tray(&pin.borrow());
        }
        WM_LBUTTONDOWN => {
            let hot = tray_hit(&pin.borrow(), pt);
            pin.borrow().tip.hide();
            // 注釈モードの帯 (トレイのいちばん下の段): 画像の上の帯と同じ操作。ドラッグはピンの窓が受ける。
            let (band, tr) = {
                let p = pin.borrow();
                (tray_layout(&p).band, p.tray_rect)
            };
            if hot.is_none()
                && let Some(b) = band
                && contains(&b, pt)
            {
                band_down(
                    pin,
                    POINT {
                        x: pt.x + tr.left,
                        y: pt.y + tr.top,
                    },
                );
                return;
            }
            if hot == Some(TrayHot::Strip) {
                set_playing(pin, false);
                {
                    let mut p = pin.borrow_mut();
                    let lay = tray_layout(&p);
                    let items = strip_items(&p.tl, p.s);
                    if let Some(i) = strip_hit(&items, p.scroll, pt.x - lay.view.left) {
                        if shift() {
                            p.tl.select_to(i);
                        } else {
                            p.tl.pick(i);
                        }
                    }
                    if shift() {
                        commit_span(&mut p, true);
                    } else {
                        p.marquee = Some(Marquee {
                            x0: pt.x - lay.view.left + p.scroll,
                            y0: pt.y,
                            cur: pt,
                            moved: false,
                            last: Instant::now(),
                            acc: 0.0,
                        });
                    }
                }
                unsafe {
                    SetCapture(hwnd);
                }
                update(pin);
                return;
            }
            pin.borrow_mut().tray_pressed = hot;
            unsafe {
                SetCapture(hwnd);
            }
            paint_tray(&pin.borrow());
        }
        // キャプチャが外れた (Alt+Tab など): ドラッグと押下を捨てる。選択はドラッグ中の値で確定。
        WM_CAPTURECHANGED => {
            let mut p = pin.borrow_mut();
            p.marquee = None;
            p.tray_pressed = None;
            paint_tray(&p);
        }
        WM_LBUTTONUP => {
            // 先に状態を取る: ReleaseCapture は WM_CAPTURECHANGED を同期で送り、状態を消す。
            let marquee = pin.borrow_mut().marquee.take();
            let pressed = pin.borrow_mut().tray_pressed.take();
            unsafe {
                let _ = ReleaseCapture();
            }
            if let Some(m) = marquee {
                {
                    let mut p = pin.borrow_mut();
                    eprintln!("gifpin: strip drag sel {:?}", p.tl.sel.map(|(a, b)| (a + 1, b + 1)));
                    commit_span(&mut p, m.moved);
                }
                let p = pin.borrow();
                paint_pin(&p);
                paint_tray(&p);
                if p.bar.visible() {
                    paint_bar(&p);
                }
                return;
            }
            let fire = {
                let p = pin.borrow();
                let hot = tray_hit(&p, pt);
                (pressed == hot).then_some(hot).flatten()
            };
            match fire {
                Some(TrayHot::Delete) if can_delete(&pin.borrow()) => delete_key(pin),
                Some(TrayHot::Ghost) => {
                    let on = {
                        let mut p = pin.borrow_mut();
                        p.ghosts = !p.ghosts;
                        p.ghosts
                    };
                    eprintln!("gifpin: out-of-span annotations {on}");
                    paint_all(pin);
                }
                Some(TrayHot::Edge) => {
                    {
                        let mut p = pin.borrow_mut();
                        if let Some((_, _, f)) = edge_mark(&p) {
                            let lay = tray_layout(&p);
                            let items = strip_items(&p.tl, p.s);
                            p.scroll =
                                follow(&items, p.scroll, lay.view.right - lay.view.left, f, false);
                            eprintln!("gifpin: edge mark -> frame {}", f + 1);
                        }
                    }
                    paint_all(pin);
                }
                Some(TrayHot::Dock) => {
                    let on = !pin.borrow().dock;
                    set_dock(pin, on);
                }
                _ => paint_tray(&pin.borrow()),
            }
        }
        WM_MOUSEWHEEL => scroll_strip(pin, (wparam.0 >> 16) as u16 as i16),
        _ => {}
    }
}

fn bar_msg(pin: &Rc<RefCell<GifPin>>, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) {
    if pin.borrow().modal && matches!(msg, WM_LBUTTONDOWN | WM_MOUSEMOVE) {
        return;
    }
    let pt = lparam_point(lparam);
    let hit = |p: &GifPin| toolbar_hit(&items(p), p.s, pt.x, pt.y);
    match msg {
        WM_MOUSEMOVE => {
            let changed = {
                let mut p = pin.borrow_mut();
                let hot = hit(&p);
                std::mem::replace(&mut p.bar_hover, hot) != hot
            };
            track_leave(hwnd);
            if changed {
                pin.borrow().tip.hide();
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_TIP);
                    if pin.borrow().bar_hover.is_some() {
                        SetTimer(Some(hwnd), TIMER_TIP, 500, None);
                    }
                }
                paint_bar(&pin.borrow());
            }
        }
        0x02A3 /* WM_MOUSELEAVE */ => {
            pin.borrow_mut().bar_hover = None;
            pin.borrow().tip.hide();
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_TIP);
            }
            paint_bar(&pin.borrow());
        }
        WM_TIMER if wparam.0 == TIMER_TIP => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_TIP);
            }
            show_bar_tip(pin);
        }
        WM_TIMER if wparam.0 == TIMER_ANIM => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_ANIM);
            }
            paint_bar(&pin.borrow());
        }
        WM_LBUTTONDOWN => {
            let hot = hit(&pin.borrow());
            pin.borrow_mut().bar_pressed = hot;
            pin.borrow().tip.hide();
            unsafe {
                SetCapture(hwnd);
            }
            paint_bar(&pin.borrow());
        }
        WM_CAPTURECHANGED => {
            pin.borrow_mut().bar_pressed = None;
            paint_bar(&pin.borrow());
        }
        WM_LBUTTONUP => {
            // 先に押下を取る: ReleaseCapture は WM_CAPTURECHANGED を同期で送り、押下を消す。
            let pressed = pin.borrow_mut().bar_pressed.take();
            unsafe {
                let _ = ReleaseCapture();
            }
            let fire = {
                let p = pin.borrow();
                let hot = hit(&p);
                (pressed == hot).then_some(hot).flatten()
            };
            // 閉じたあとは描かない (窓ごと消えている)。
            if fire == Some(I_CLOSE) {
                close(pin);
                return;
            }
            if let Some(i) = fire {
                bar_action(pin, i);
            }
            paint_bar(&pin.borrow());
        }
        // 拡大率のボタンの上のホイールは、ボタンが逃げないよう画像の右上を基準に拡縮する。
        WM_MOUSEWHEEL if pin.borrow().bar_hover == Some(I_ZOOM) && pin.borrow().edit.is_none() => {
            let delta = (wparam.0 >> 16) as u16 as i16;
            let (z, img) = {
                let p = pin.borrow();
                (p.zoom, p.img)
            };
            set_zoom(pin, wheel_zoom(z, delta), zoom_anchor(img));
        }
        _ => {}
    }
}

/// ポップアップ: ドロップダウンのホバーと、押して一覧の開け閉め (ほかの所を押すと一覧を閉じる)。
fn pop_msg(pin: &Rc<RefCell<GifPin>>, hwnd: HWND, msg: u32, lparam: LPARAM) {
    if pin.borrow().modal && matches!(msg, WM_LBUTTONDOWN | WM_MOUSEMOVE) {
        return;
    }
    let pt = lparam_point(lparam);
    let ann = pin.borrow().ann_pop;
    if let Some(c) = ann {
        match msg {
            WM_MOUSEMOVE => {
                track_leave(hwnd);
                let mut p = pin.borrow_mut();
                let hot = annot_edit::popup_hit(c, pt.x, pt.y, p.s);
                if std::mem::replace(&mut p.ann_hover, hot) != hot {
                    paint_pop(&p);
                }
            }
            0x02A3 /* WM_MOUSELEAVE */ => {
                let mut p = pin.borrow_mut();
                if p.ann_hover.take().is_some() {
                    paint_pop(&p);
                }
            }
            WM_LBUTTONDOWN => {
                let hot = annot_edit::popup_hit(c, pt.x, pt.y, pin.borrow().s);
                if let Some(i) = hot {
                    choose(pin, c, i);
                }
            }
            _ => {}
        }
        return;
    }
    let on_dd = |p: &GifPin| {
        let dds = pop_layout(p.s).2;
        [Dd::Scale, Dd::Limit]
            .into_iter()
            .find(|&k| contains(&dds[k as usize], pt))
    };
    match msg {
        WM_MOUSEMOVE => {
            // 離れの追跡は移動のときだけ入れる (KNOWLEDGE「WM_MOUSELEAVE の投函の連鎖」)。
            track_leave(hwnd);
            let mut p = pin.borrow_mut();
            let hot = on_dd(&p);
            if std::mem::replace(&mut p.dd_hover, hot) != hot {
                paint_pop(&p);
            }
        }
        0x02A3 /* WM_MOUSELEAVE */ => {
            let mut p = pin.borrow_mut();
            if p.dd_hover.take().is_some() {
                paint_pop(&p);
            }
        }
        WM_LBUTTONDOWN => {
            let mut p = pin.borrow_mut();
            // 開いている一覧のドロップダウンをもう一度押すと閉じる。別のを押せば開き直す。
            let open = on_dd(&p).filter(|&k| p.list_open != Some(k));
            if open.is_some() || p.list_open.is_some() {
                set_list(&mut p, open);
            }
        }
        _ => {}
    }
}

/// 倍率の一覧: ホバーの行を追い、離した行の倍率を選ぶ (ドロップダウンを押したまま行で離しても選ぶ)。
fn list_msg(pin: &Rc<RefCell<GifPin>>, hwnd: HWND, msg: u32, lparam: LPARAM) {
    if pin.borrow().modal && matches!(msg, WM_LBUTTONUP | WM_MOUSEMOVE) {
        return;
    }
    let pt = lparam_point(lparam);
    let hit = |p: &GifPin| {
        let w = p.list_rect.right - p.list_rect.left;
        let n = match p.list_open {
            _ if p.zoom_open => ZOOMS.len(),
            Some(k) => k.values().len(),
            None => 0,
        };
        widgets::list_hit(n, w, pt.x, pt.y, p.s)
    };
    match msg {
        WM_MOUSEMOVE => {
            track_leave(hwnd);
            let mut p = pin.borrow_mut();
            let row = hit(&p);
            if row.is_some() && std::mem::replace(&mut p.list_hover, row) != row {
                paint_pop(&p);
                paint_zoom_list(&p);
            }
        }
        WM_LBUTTONUP if pin.borrow().zoom_open => {
            let row = hit(&pin.borrow());
            if let Some(i) = row {
                let img = {
                    let mut p = pin.borrow_mut();
                    set_zoom_list(&mut p, false);
                    paint_bar(&p);
                    p.img
                };
                set_zoom(pin, ZOOMS[i] as f32 / 100.0, zoom_anchor(img));
            }
        }
        WM_LBUTTONUP => {
            let open = pin.borrow().list_open;
            let row = open.and_then(|k| Some((k, hit(&pin.borrow())?)));
            if let Some((k, i)) = row {
                choose_export(pin, k, k.values()[i]);
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn hud_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_SETCURSOR => {
            // 注釈モードの帯 (トレイの段) の下の段のつまみの上は左右の矢印。
            let we = find_hud(hwnd).is_some_and(|(pin, role)| {
                role == Role::Tray
                    && pin
                        .try_borrow()
                        .is_ok_and(|p| rail_hit(&p, cursor()) == Some(true))
            });
            unsafe {
                SetCursor(LoadCursorW(None, if we { IDC_SIZEWE } else { IDC_ARROW }).ok());
            }
            return LRESULT(1);
        }
        WM_PAINT => {
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            if let Some((pin, Role::Tray)) = find_hud(hwnd) {
                edge_frame(&pin, hwnd);
            }
            return LRESULT(0);
        }
        _ => {}
    }
    let Some((pin, role)) = find_hud(hwnd) else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    let handled = matches!(
        msg,
        WM_MOUSEMOVE
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_MOUSEWHEEL
            | WM_TIMER
            | WM_CAPTURECHANGED
            | 0x02A3
    );
    if !handled {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    match role {
        Role::Tray => tray_msg(&pin, hwnd, msg, wparam, lparam),
        Role::Bar => bar_msg(&pin, hwnd, msg, wparam, lparam),
        Role::Pop => pop_msg(&pin, hwnd, msg, lparam),
        Role::List => list_msg(&pin, hwnd, msg, lparam),
        Role::Tip => {}
    }
    LRESULT(0)
}

unsafe extern "system" fn pin_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let Some(pin) = find(hwnd) else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    match msg {
        WM_NCHITTEST => {
            let pt = lparam_point(lparam);
            let inside = pin.try_borrow().is_ok_and(|p| contains(&p.img, pt));
            if !inside {
                // 影の余白は下の窓へ通す (別プロセスの窓へも)。
                widgets::pass_margin(hwnd);
            }
            LRESULT(if inside {
                HTCLIENT as isize
            } else {
                HTTRANSPARENT as isize
            })
        }
        WM_SETCURSOR => {
            let id = pin.try_borrow().map_or(IDC_SIZEALL, |p| {
                if p.edit.is_some() {
                    annot_edit::cursor_id(&*p)
                } else if rail_hit(&p, cursor()) == Some(true) {
                    IDC_SIZEWE
                } else if p.timeline && contains(&band_of(&p).0, cursor()) {
                    IDC_ARROW
                } else {
                    IDC_SIZEALL
                }
            });
            unsafe {
                SetCursor(LoadCursorW(None, id).ok());
            }
            LRESULT(1)
        }
        WM_LBUTTONDOWN if pin.borrow().edit.is_some() => {
            {
                let mut p = pin.borrow_mut();
                if p.ann_pop.is_some() {
                    set_ann_pop(&mut p, None);
                    paint_bar(&p);
                }
            }
            annot_edit::down(&(pin.clone() as HostRef));
            // 選んだ注釈が替わると、トレイの区間・状態・削除のツールチップが替わる。
            paint_tray(&pin.borrow());
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let c = cursor();
            if !band_down(&pin, c) {
                {
                    let mut p = pin.borrow_mut();
                    p.drag = Some(Drag::Move(POINT {
                        x: c.x - p.img.left,
                        y: c.y - p.img.top,
                    }));
                }
                unsafe {
                    SetCapture(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if annot_edit::drag(&*pin) {
                paint_pin(&pin.borrow());
                return LRESULT(0);
            }
            let c = cursor();
            let drag = pin.borrow().drag;
            match drag {
                Some(Drag::Move(off)) => {
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
                }
                Some(Drag::Scroll(off)) => {
                    {
                        let mut p = pin.borrow_mut();
                        let band = band_of(&p).0;
                        rail_scroll_to(&mut p, c.x - band.left - off);
                    }
                    paint_all(&pin);
                }
                Some(d) => {
                    {
                        let mut p = pin.borrow_mut();
                        let (band, g) = band_of(&p);
                        let n = p.tl.len();
                        apply_band_drag(&mut p, d, frame_at(c.x - band.left, n, &g));
                    }
                    update(&pin);
                }
                None => {
                    show_bar(&pin);
                    set_timeline(&pin, true);
                    let hover = {
                        let p = pin.borrow();
                        let (band, g) = band_of(&p);
                        contains(&offset(g.play, band.left, band.top), c)
                    };
                    let changed = std::mem::replace(&mut pin.borrow_mut().play_hover, hover) != hover;
                    track_leave(hwnd);
                    if changed {
                        paint_pin(&pin.borrow());
                    }
                    // 注釈モードの帯はトレイの窓が受ける。
                    if pin.borrow().edit.is_none() {
                        let rail = rail_hit(&pin.borrow(), c).is_some();
                        set_rail_hover(&pin, hwnd, rail);
                    }
                }
            }
            LRESULT(0)
        }
        0x02A3 /* WM_MOUSELEAVE */ => {
            if std::mem::take(&mut pin.borrow_mut().play_hover) {
                paint_pin(&pin.borrow());
            }
            if pin.borrow().edit.is_none() {
                set_rail_hover(&pin, hwnd, false);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_TIP => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_TIP);
            }
            show_rail_tip(&pin);
            LRESULT(0)
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED => {
            annot_edit::up(&*pin);
            let mut scrolled = false;
            if let Ok(mut p) = pin.try_borrow_mut() {
                scrolled = matches!(p.drag.take(), Some(Drag::Scroll(_)));
                if scrolled {
                    eprintln!("gifpin: strip scroll {} (thumb)", p.scroll);
                    // 離した所が下の段の外なら、ホバーの見た目を戻す。
                    p.rail_hover = rail_hit(&p, cursor()).is_some();
                }
            }
            if msg == WM_LBUTTONUP {
                unsafe {
                    let _ = ReleaseCapture();
                }
            }
            if scrolled {
                paint_all(&pin);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            show_bar(&pin);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = (wparam.0 >> 16) as u16 as i16;
            let over_tray = {
                let p = pin.borrow();
                p.timeline && contains(&p.tray_rect, cursor())
            };
            if over_tray {
                scroll_strip(&pin, delta);
            } else {
                let z = pin.borrow().zoom;
                set_zoom(&pin, wheel_zoom(z, delta), cursor());
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            key_down(&pin, wparam.0 as u16);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_PLAY => {
            play_tick(&pin);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_BAR => {
            timeline_tick(&pin);
            bar_tick(&pin);
            LRESULT(0)
        }
        WM_VIS_FRAME => {
            let p = pin.borrow();
            if p.edit.is_some() {
                paint_tray(&p);
            } else {
                paint_pin(&p);
            }
            // 描き終えてから下ろす: 糸は次の更新まで投函しないので、列が空く間に WM_TIMER (再生の刻み) が出る。
            // 描く前に下ろすと、描くのが 1 更新に近い大きい GIF では列が空かず、再生が数コマ止まった (2026-11-08)。
            p.vis_pending.store(false, Ordering::Release);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_BUSY => {
            busy_tick(&pin);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_STATUS => {
            status_tick(&pin);
            LRESULT(0)
        }
        WM_GIF_DONE => {
            finish_export(&pin);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            set_scale(&pin, (wparam.0 & 0xFFFF) as u32);
            LRESULT(0)
        }
        WM_PAINT => {
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            {
                let mut p = pin.borrow_mut();
                // 入力の欄は持ち主と一緒に消える。GDI の物だけ返す。
                if let Some(e) = p.edit.as_mut() {
                    annot_edit::release_text(e);
                }
                unsafe {
                    if p.playing.take().is_some() {
                        timeEndPeriod(1);
                    }
                    p.edge_gen.store(0, Ordering::Release);
                    let _ = KillTimer(Some(hwnd), TIMER_PLAY);
                    let _ = KillTimer(Some(hwnd), TIMER_BAR);
                    let _ = KillTimer(Some(hwnd), TIMER_BUSY);
                    let _ = KillTimer(Some(hwnd), TIMER_STATUS);
                    for a in [&p.tray, &p.tip, &p.bar, &p.pop, &p.list] {
                        let _ = DestroyWindow(a.hwnd);
                    }
                }
                for f in p.temp.drain(..) {
                    match std::fs::remove_file(&f) {
                        Ok(()) => eprintln!("gifpin: removed {}", f.display()),
                        Err(e) => eprintln!("gifpin: remove {} failed: {e}", f.display()),
                    }
                }
            }
            PINS.with(|p| p.borrow_mut().retain(|x| !Rc::ptr_eq(x, &pin)));
            eprintln!("gifpin: closed");
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

impl annot_edit::Host for GifPin {
    fn window(&self) -> HWND {
        self.hwnd
    }
    fn tag(&self) -> &'static str {
        "gifpin"
    }
    fn img(&self) -> RECT {
        self.img
    }
    fn scale(&self) -> f32 {
        self.s
    }
    fn image(&self) -> &Image {
        &self.frame
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
        paint_pin(self);
    }
    /// エディタが `Doc` に手を積んだ後にも呼ばれる。呼び手は `borrow()` を持っているので、ここでは
    /// GifPin を借り直さない (`ann` の RefCell だけを借りる)。
    fn toolbar_changed(&self) {
        sync_ann(self);
        paint_pin(self);
        if self.bar.visible() {
            paint_bar(self);
        }
        paint_tray(self);
    }
    fn vis(&self, i: usize) -> Vis {
        vis(self, i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// カード (GifPin README「寸法と置き方」) の 100% の値と、150% でその 1.5 倍 (丸め ±1)。
    #[test]
    fn export_popup_geometry_follows_card() {
        for s in [1.0f32, 1.5] {
            let near = |got: i32, card: f32, what: &str| {
                let want = card * s;
                let tol = if s == 1.0 { 0.0 } else { 1.0 };
                assert!(
                    (got as f32 - want).abs() <= tol,
                    "{what} at {s}: {got} vs {want}"
                );
            };
            let (layout, bar_w) = toolbar_layout(&ITEMS, s);
            // DS「Odile」GifPin の README「ツールバーの並び」: 注釈の左端は外形の左から 121、オプションは 170。
            near(layout[I_ANNOTATE].0, 121.0, "annotate x");
            near(layout[I_OPT].0, 170.0, "option x");
            near(layout[I_OPT].1, 32.0, "option w");
            near(toolbar_height(s), 40.0, "toolbar h");
            // 外形は DS の GifPin のツールバーと同じ 362 (拡大率のボタン 56 を含む。t261107-d93c)。
            // 150%: 区切り 4 つの 13 × 1.5 = 19.5 がそれぞれ 20 に丸まるので、543 でなく 545。
            if s == 1.0 {
                near(bar_w, 362.0, "toolbar w");
            } else {
                assert_eq!(bar_w, 545);
            }
            let (pw, ph, dd) = pop_layout(s);
            near(pw, 248.0, "popup w");
            near(ph, 134.0, "popup h");
            near(dd[1].top, 86.0, "limit dropdown y");
            let dd = dd[0];
            near(dd.left, 88.0, "dropdown x");
            near(dd.top, 42.0, "dropdown y");
            near(dd.right - dd.left, 144.0, "dropdown w");
            near(dd.bottom - dd.top, 32.0, "dropdown h");
            near(pw - dd.right, 16.0, "dropdown right");
            let (lw, lh) = widgets::list_size(GIF_SCALES.len(), dd.right - dd.left, s);
            near(lw, 180.0, "list w");
            near(lh, 176.0, "list h");
        }
    }

    /// GifPinAnnotate のカードの「寸法と置き方」(100% の px、トレイの左上から) と、通常モードの 72。
    #[test]
    fn tray_layout_follows_card() {
        let r = |left, top, right, bottom| RECT {
            left,
            top,
            right,
            bottom,
        };
        let n = tray_layout_for(560, 1.0, false);
        assert_eq!((n.w, n.h, n.band), (560, 72, None));
        assert_eq!(n.view, r(8, 26, 552, 72));
        let e = tray_layout_for(560, 1.0, true);
        // 108 = 状態の段 26 + サムネイルの段 38 + シークバー 44。
        assert_eq!((e.w, e.h), (560, 108));
        // 削除は右から 8・上から 4、区間外の注釈のトグルは削除の左 4 (右端は右から 34)。
        assert_eq!(e.del, r(530, 4, 552, 26));
        assert_eq!(e.dock, r(504, 4, 526, 26));
        // フィルムストリップの窓は上から 26・左右 8・高さ 42、シークバーの段は上から 64・高さ 44。
        assert_eq!(e.view, r(8, 26, 552, 68));
        assert_eq!(e.band, Some(r(0, 64, 560, 108)));
        // 幅は画像と同じで、400 未満なら 400。
        assert_eq!(tray_layout_for(300, 1.0, true).w, 400);
        // 150%: 108 × 1.5 = 162、シークバーの段は 64 × 1.5 = 96 から。
        let e = tray_layout_for(840, 1.5, true);
        assert_eq!(
            (e.h, e.band.map(|b| (b.top, b.bottom))),
            (162, Some((96, 162)))
        );
    }
}
