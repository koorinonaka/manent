//! 設定窓 (DS「Odile」SettingsWindow): トレイの「設定…」で開く。窓は 1 つだけ。
//! 左にナビ (一般 / 保存 / 録画 / ホットキー)、右に全セクションの行を縦にスクロールする 1 枚のペイン。
//! ナビはセクションへのジャンプで (滑らかにスクロールする)、印はスクロールに追従する。値を変えたらすぐ保存して反映する (適用ボタンは無い)。
//! 部品は遷移する (ホバー・トグル・一覧の開き・ナビの印)。Windows の「アニメーション効果」がオフなら遷移しない。
//! 地はアクティブの間 DWM acrylic (透明に描いて DWM に任せる)、非アクティブは `surface-opaque`。
//! タイトルバーは OS が描く。最小化・最大化でき、枠をつまんで大きさを変えられる (最初は 708 × 648、最小は 680 × 240)。UI はメインスレッドだけで動く。

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Instant;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::{D2D1_ANTIALIAS_MODE_ALIASED, D2D1_LAYER_PARAMETERS1};
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient, ValidateRect};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_DOWN, VK_ESCAPE,
    VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HSTRING, PCWSTR, w};

use crate::hotkey::{self, Recorded, Recorder};
use crate::ui::anim::{self, Anim, Curve};
use crate::ui::widgets::{self, Acrylic, Capture, HotkeyField};
use crate::ui::{
    self, Style, Surface, contains, draw_text, fill_rect, fill_round, lparam_point, px, rectf, rf,
    stroke, track_leave,
};

const CLASS: PCWSTR = w!("OdileSettings");
const LIST_CLASS: PCWSTR = w!("OdileSettingsList");

const NAV_ICONS: [&str; 4] = ["sliders", "folder", "video", "keyboard"];
const LANGS: [&str; 3] = ["auto", "ja", "en"];
const FPS: [u32; 5] = [10, 15, 20, 30, 60];
use crate::settings::COUNTDOWNS;
/// 録画の上限の選択肢 (秒)。
const LIMITS: [u32; 7] = [10, 15, 30, 60, 120, 180, 300];

// 遷移の時間 (ms)。transitions.dev の型 (ホバーは戻りも遷移、つまみは小さく弾んで滑る、一覧は出どころから開く) に
// 合わせてこの窓で決めた値 (サイトは数値を出していない)。
const TIMER_ANIM: usize = 1;
const HOVER_MS: f32 = 120.0;
const TOGGLE_MS: f32 = 220.0;
const MARK_MS: f32 = 200.0;
const LIST_MS: f32 = 160.0;
const SCROLL_MS: f32 = 280.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ctl {
    /// ACTIONS の添字 (静止画・動画)。
    Hotkey(usize),
    Takeover,
    Autostart,
    ExplorerMenu,
    Language,
    CursorStill,
    /// ピンをキャプチャに写す (`show_in_capture`)。
    ShowInCapture,
    Fps,
    Limit,
    CursorVideo,
    /// 録画前のカウントダウン (`record_countdown`)。
    Countdown,
    SaveDir,
    /// ms-screenclip の既定の状態と、Windows の既定のアプリを開くボタン。
    Screenclip,
    /// 更新の確認 (`update_check`)。
    UpdateCheck,
    /// 更新の確認の結果と、新しい版があれば「更新」か「ダウンロードページを開く」のボタン。
    Update,
}

/// 右ペインの全行と、その行のセクション (0 一般、1 保存、2 録画、3 ホットキー。ナビと同じ順)。
const ROWS: [(Ctl, usize); 16] = [
    (Ctl::Autostart, 0),
    (Ctl::ExplorerMenu, 0),
    (Ctl::Language, 0),
    (Ctl::CursorStill, 0),
    (Ctl::ShowInCapture, 0),
    (Ctl::UpdateCheck, 0),
    (Ctl::Update, 0),
    (Ctl::SaveDir, 1),
    (Ctl::Fps, 2),
    (Ctl::Limit, 2),
    (Ctl::CursorVideo, 2),
    (Ctl::Countdown, 2),
    (Ctl::Hotkey(0), 3),
    (Ctl::Hotkey(1), 3),
    (Ctl::Screenclip, 3),
    // 最後の行に置く: 隠すときは末尾を削るだけにし、ほかの行の添字 (フォーカス・トグルの遷移) を動かさない。
    (Ctl::Takeover, 3),
];

/// ストア版の行: 更新の 2 行が無い (更新はストアに任せる)。
static STORE_ROWS: LazyLock<Vec<(Ctl, usize)>> = LazyLock::new(|| {
    ROWS.into_iter()
        .filter(|r| !matches!(r.0, Ctl::UpdateCheck | Ctl::Update))
        .collect()
});

/// 今出す行。ms-screenclip の既定が Manent で横取りが OFF なら、「PrintScreen キーで撮る」は要らないので隠す。
/// 横取りが ON なら隠さない (フックがキーを奪い続けるのに、切る手段が消えるため)。
fn rows(st: &State) -> &'static [(Ctl, usize)] {
    let all: &'static [(Ctl, usize)] = if crate::is_store() {
        &STORE_ROWS
    } else {
        &ROWS
    };
    if st.hide_takeover {
        &all[..all.len() - 1]
    } else {
        all
    }
}

/// `hide_takeover` を今の状態で決め直す。窓がアクティブになったときだけ呼ぶ
/// (窓の中で横取りを切った瞬間に、行がカーソルの下から消えないように)。
fn refresh_clip(st: &mut State) {
    st.clip_ours = crate::screenclip_is_ours();
    st.hide_takeover = st.clip_ours && !crate::settings().printscreen_takeover;
    st.startup_on = startup_on();
}

/// ストア版の自動起動のトグルに出す `StartupTask` の状態 (「設定 > スタートアップ」で切られたら OFF)。
/// GitHub 版は使わない (設定の値を出す)。描画のたびに WinRT を呼ばないよう `State` に覚える。
fn startup_on() -> bool {
    crate::is_store() && crate::startup_task_enabled()
}

/// ナビの印のセクション: アンカーがスクロールの量以下にある最後のセクション。
/// いちばん下 (最大値) では最後のセクション (短くてラベルが上端まで届かないため)。
fn spy(anchors: &[i32], scroll: i32, max: i32) -> usize {
    if scroll >= max {
        return anchors.len() - 1;
    }
    anchors.iter().rposition(|&a| a <= scroll).unwrap_or(0)
}

/// ナビを押した先。最大値を超えるアンカーは最大値にする。
fn jump(anchor: i32, max: i32) -> i32 {
    anchor.clamp(0, max)
}

/// [top, bottom] (右ペインの上からの px) を上下 `margin` の内側に入れる最小のスクロールの差。
fn into_view(top: i32, bottom: i32, vis: i32, margin: i32) -> i32 {
    if top < margin {
        top - margin
    } else if bottom > vis - margin {
        bottom - (vis - margin)
    } else {
        0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Nav,
    Row(usize),
}

/// ホバー。`Row` は行の部品、`Bar` は右端のスクロールバー。
/// 選べる文字: セクションのラベル、行のラベル、行の説明。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TextItem {
    Section(usize),
    Label(usize),
    Desc(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Hot {
    Nav(usize),
    Row(usize),
    Bar,
}

/// 遷移する値の名前。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Ak {
    Hover(Hot),
    /// `rows(st)` の添字のトグルの位置 (オフ 0、オン 1)。
    Toggle(usize),
    /// ナビの印の行 (小数で途中)。
    Mark,
    /// 一覧の開き (0 から 1)。
    List,
}

struct State {
    hwnd: HWND,
    surface: Surface,
    list: Acrylic,
    backdrop: bool,
    s: f32,
    /// 右ペインのスクロールの量 (物理 px)。`layout` が 0..=最大値 に収める。
    scroll: i32,
    /// 最後にログへ出したナビの印のセクション (`mark_of`)。
    spied: usize,
    /// ナビで押したセクション (行の押下か、ナビにフォーカスがあるときの ↑ ↓)。印はここに付く。
    /// ナビ以外でスクロールの量が変わったら消す (`scroll_to`)。
    picked: Option<usize>,
    /// つまみをドラッグ中なら、押した点とつまみの上端の差。
    drag: Option<i32>,
    /// ナビで押した先へ滑らかにスクロール中 (始めの量、先の量、始めた時刻)。
    glide: Option<(i32, i32, Instant)>,
    /// 遷移中の値。描画 (`&State`) の中で目標を差し替えるので RefCell。
    tweens: RefCell<Anim<Ak>>,
    /// 遷移のタイマーを張っている。
    ticking: Cell<bool>,
    /// Windows の「アニメーション効果」(SPI_GETCLIENTAREAANIMATION)。オフなら遷移しない。
    motion: bool,
    focus: Focus,
    /// キーボードで動かしたらフォーカス表示を出す。クリックで消す。
    focus_visible: bool,
    hover: Option<Hot>,
    /// 記録中のホットキー (添字と記録の状態)。
    recording: Option<(usize, Recorder)>,
    /// 記録で確定したキーとその文字列。キーを離すまで登録しない (押し続けの自動の繰り返しが
    /// 新しいホットキーを撃って凍結が始まるため)。その間も記録のフックがキーを飲む。
    held: Option<(u32, String)>,
    /// 登録に失敗したホットキーの添字とエラー文。
    error: Option<(usize, String)>,
    /// 記録中に押したキーを割り当てられなかった理由。記録の終わりで消す。
    rec_note: Option<String>,
    /// 選んだ文字 (`TextItem` の中の UTF-16 の範囲。anchor が押した所、head が今の所)。
    sel: Option<(TextItem, u32, u32)>,
    /// 文字をドラッグで選んでいる。
    selecting: bool,
    /// 前に文字を押した時刻と位置 (ダブルクリックの判定)。
    last_click: Option<(Instant, i32, i32)>,
    /// ms-screenclip の既定が Manent か。窓がアクティブになるたびに読み直す
    /// (Windows の設定で選んで戻ってきたときに変わる)。
    clip_ours: bool,
    /// 「PrintScreen キーで撮る」の行を隠すか (`refresh_clip`)。
    hide_takeover: bool,
    /// ストア版の自動起動のトグルの値 (`startup_on`)。窓がアクティブになるたびと、トグルを押した後に読み直す。
    startup_on: bool,
    /// 開いている一覧 (言語・フレームレート・録画の上限) と、その中のホバーかキーボードの行。
    list_open: Option<Ctl>,
    /// 一覧を部品の上に出した (下に入らなかった)。開くときに部品の側から滑らせる向き。
    list_up: bool,
    list_hover: Option<usize>,
    list_rect: RECT,
    active: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// 状態があり、借りられれば `f` を呼ぶ。窓のメッセージは入れ子で届くことがあるので try で借りる。
fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|st| match st.try_borrow_mut() {
        Ok(mut g) => g.as_mut().map(f),
        Err(_) => None,
    })
}

fn hwnd_of() -> Option<HWND> {
    STATE.with(|st| {
        st.try_borrow()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.hwnd))
    })
}

/// 値 `k` を `target` へ遷移させ、今の値を返す。初めて見る値は `init` から始める。
/// 目標が変わったら今の値から遷移し直す。遷移中ならタイマー (`tick`) を張る。
fn ease(st: &State, k: Ak, target: f32, ms: f32, c: Curve, init: f32) -> f32 {
    let v = st
        .tweens
        .borrow_mut()
        .enter_with(k, target, ms, st.motion, init, c);
    if st.tweens.borrow().busy() {
        run_anim(st);
    }
    v
}

fn run_anim(st: &State) {
    if !st.ticking.replace(true) {
        unsafe {
            SetTimer(Some(st.hwnd), TIMER_ANIM, 15, None);
        }
    }
}

/// 遷移の 1 コマ: 滑らかなスクロールを進めて描き直す。全部終わったらタイマーを外す。
fn tick(st: &mut State) {
    if let Some((from, to, t0)) = st.glide {
        let p = t0.elapsed().as_secs_f32() * 1000.0 / SCROLL_MS;
        let y = from + ((to - from) as f32 * anim::ease_out(p)).round() as i32;
        move_to(st, y);
        log_mark(st);
        if p < 1.0 {
            st.glide = Some((from, to, t0));
        }
    }
    paint(st);
    paint_list(st);
    let busy = st.glide.is_some() || st.tweens.borrow().busy();
    if !busy {
        st.ticking.set(false);
        unsafe {
            let _ = KillTimer(Some(st.hwnd), TIMER_ANIM);
        }
    }
}

/// ナビの印のセクション。ナビで押したらそのセクション (滑っている間も)。ほかはスクロールの量から (`spy`)。
fn mark_of(st: &State, lay: &Lay) -> usize {
    if let Some(i) = st.picked {
        return i;
    }
    match st.glide {
        Some((_, to, _)) => spy(&lay.anchors, to, lay.max),
        None => lay.spy(),
    }
}

/// トレイの「設定…」。開いていれば前面に出す。
pub fn show() {
    if let Some(hwnd) = hwnd_of() {
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
        }
        crate::force_foreground(hwnd);
        eprintln!("settings window: brought to front");
        return;
    }
    match create() {
        Ok(hwnd) => {
            // ShowWindow ではなく SetWindowPos で出す: 最初の ShowWindow は起動時の表示状態
            // (STARTUPINFO。非表示で起動されたら SW_HIDE) に置き換えられる。
            fit(hwnd);
            crate::force_foreground(hwnd);
            log_bounds(hwnd);
            with(|st| log_scroll(&layout(st), st.s));
        }
        Err(e) => eprintln!("settings window: create failed: {e}"),
    }
}

fn create() -> windows::core::Result<HWND> {
    widgets::register_class(CLASS, wndproc);
    widgets::register_class(LIST_CLASS, list_proc);
    let t = crate::strings();
    unsafe {
        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP,
            CLASS,
            &HSTRING::from(t.settings_title),
            // タイトルバーに最小化・最大化・閉じる。枠で大きさを変えられる。
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            708,
            648,
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
        let backdrop =
            widgets::set_attr(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &DWMSBT_TRANSIENTWINDOW).is_ok();
        let _ = widgets::set_attr(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_ROUND);
        set_dark(hwnd);
        crate::pin::prevent_pinning(hwnd);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let surface = Surface::new(hwnd, rc.right as u32, rc.bottom as u32)?;
        let list = Acrylic::new(LIST_CLASS, false, Capture::Shown)?;
        eprintln!("settings window: created (backdrop attribute accepted: {backdrop})");
        STATE.with(|st| {
            *st.borrow_mut() = Some(State {
                hwnd,
                surface,
                list,
                backdrop,
                s: ui::scale_of(hwnd),
                scroll: 0,
                spied: 0,
                picked: None,
                drag: None,
                glide: None,
                tweens: RefCell::new(Anim::default()),
                ticking: Cell::new(false),
                motion: anim::motion(),
                focus: Focus::Nav,
                focus_visible: false,
                hover: None,
                recording: None,
                held: None,
                error: None,
                rec_note: None,
                sel: None,
                selecting: false,
                last_click: None,
                // refresh_clip と同じ。最初の描画から合わせる (アクティブ化を待つと行が一瞬出る)。
                clip_ours: crate::screenclip_is_ours(),
                hide_takeover: crate::screenclip_is_ours()
                    && !crate::settings().printscreen_takeover,
                startup_on: startup_on(),
                list_open: None,
                list_up: false,
                list_hover: None,
                list_rect: RECT::default(),
                active: false,
            })
        });
        Ok(hwnd)
    }
}

fn set_dark(hwnd: HWND) {
    let _ = widgets::set_attr(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &BOOL::from(ui::apps_dark()),
    );
    // タイトルバーは非クライアントで、クライアントの地を塗っても acrylic のまま (ダークの白い背景で字が 3.1:1)。
    // DWM の地の色を `surface-opaque` にする。COLORREF は 0x00BBGGRR。
    let c = ui::palette().surface_opaque;
    let byte = |v: f32| (v * 255.0).round() as u32;
    let _ = widgets::set_attr(
        hwnd,
        DWMWA_CAPTION_COLOR,
        &COLORREF(byte(c.r) | byte(c.g) << 8 | byte(c.b) << 16),
    );
}

/// 見える外形 (`DWMWA_EXTENDED_FRAME_BOUNDS`) を 708 × 648 (×s) にして、作業領域の中央に出す。
fn fit(hwnd: HWND) {
    let s = ui::scale_of(hwnd);
    let (tw, th) = (px(708.0, s), px(648.0, s));
    unsafe {
        // 2 回: 1 回目で枠の見えない部分の幅を測り、2 回目で合わせる。
        for _ in 0..2 {
            let mut wr = RECT::default();
            let _ = GetWindowRect(hwnd, &mut wr);
            let ext = ui::frame_bounds(hwnd);
            let (ml, mt) = (ext.left - wr.left, ext.top - wr.top);
            let (mr, mb) = (wr.right - ext.right, wr.bottom - ext.bottom);
            let work = ui::monitor_at(ui::cursor()).work;
            let _ = SetWindowPos(
                hwnd,
                None,
                (work.left + work.right - tw) / 2 - ml,
                (work.top + work.bottom - th) / 2 - mt,
                tw + ml + mr,
                th + mt + mb,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }
}

/// 枠の見えない部分を足した、窓の最小の大きさ (見える外形で 680 × 240、×s)。
fn min_track(hwnd: HWND) -> POINT {
    let s = ui::scale_of(hwnd);
    let mut wr = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut wr);
    }
    let ext = ui::frame_bounds(hwnd);
    POINT {
        x: px(680.0, s) + (wr.right - wr.left) - (ext.right - ext.left),
        y: px(240.0, s) + (wr.bottom - wr.top) - (ext.bottom - ext.top),
    }
}

/// 実機検証用: 外形・クライアント・タイトルバーの高さをログに出す。
fn log_bounds(hwnd: HWND) {
    let mut rc = RECT::default();
    let mut origin = POINT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
        let _ = ClientToScreen(hwnd, &mut origin);
    }
    let ext = ui::frame_bounds(hwnd);
    eprintln!(
        "settings window: frame {}x{} at ({}, {}), client {}x{}, title bar {} (scale {})",
        ext.right - ext.left,
        ext.bottom - ext.top,
        ext.left,
        ext.top,
        rc.right,
        rc.bottom,
        origin.y - ext.top,
        ui::scale_of(hwnd)
    );
}

// ---- 配置 ----

/// 位置はクライアントの座標 (スクロールを引いた後)。
struct RowLay {
    ctl: Ctl,
    sec: usize,
    /// セクションの最初の行 (ラベルのすぐ下。上の `hairline` を引かない)。
    first: bool,
    top: i32,
    bottom: i32,
    /// ラベルと部品の段の上端と高さ (ヒントの段を除く)。
    band_top: i32,
    band: i32,
    /// 部品の外形 (フォーカス表示の対象)。
    ctrl: RECT,
    /// 押せる範囲 (トグルはオン/オフの文字も含む)。
    hit: RECT,
    label: ui::Text,
    desc: Option<ui::Text>,
}

struct Lay {
    nav: [RECT; 4],
    /// 右ペインの左辺 (ナビの右辺)。
    pane_l: i32,
    content_l: i32,
    content_r: i32,
    rows: Vec<RowLay>,
    /// セクションのラベルと、その上端 (クライアントの座標)。
    labels: Vec<(ui::Text, i32)>,
    /// ホットキーのセクションのヒントの段の上端 (クライアントの座標)。
    hint_top: i32,
    /// ナビで押したときのスクロールの量 (ラベルの上端 − 32。最初は 0)。
    anchors: [i32; 4],
    /// 内容の高さ、右ペインの見える高さ、スクロールの最大値、収めたスクロールの量。
    content_h: i32,
    vis: i32,
    max: i32,
    scroll: i32,
}

impl RowLay {
    /// ラベルの上端 (ラベルと説明を段の中央に置く)。
    fn label_y(&self) -> f32 {
        let lab_h = self.label.h + self.desc.as_ref().map_or(0.0, |d| d.h);
        self.band_top as f32 + ((self.band as f32 - lab_h) / 2.0).round()
    }
}

impl Lay {
    /// 選べる文字とその左上。
    fn text(&self, item: TextItem) -> Option<(&ui::Text, f32, f32)> {
        let x = self.content_l as f32;
        match item {
            TextItem::Section(i) => self.labels.get(i).map(|(t, top)| (t, x, *top as f32)),
            TextItem::Label(k) => self.rows.get(k).map(|r| (&r.label, x, r.label_y())),
            TextItem::Desc(k) => {
                let r = self.rows.get(k)?;
                Some((r.desc.as_ref()?, x, r.label_y() + r.label.h))
            }
        }
    }

    /// 点の下の選べる文字。
    fn text_hit(&self, x: i32, y: i32) -> Option<TextItem> {
        let (x, y) = (x as f32, y as f32);
        (0..self.labels.len())
            .map(TextItem::Section)
            .chain((0..self.rows.len()).flat_map(|k| [TextItem::Label(k), TextItem::Desc(k)]))
            .find(|&item| {
                self.text(item)
                    .is_some_and(|(t, l, top)| x >= l && x < l + t.w && y >= top && y < top + t.h)
            })
    }

    fn spy(&self) -> usize {
        spy(&self.anchors, self.scroll, self.max)
    }

    /// スクロールバーのつまみの上端と長さ (右ペインの上から)。通り道は上下 4 の内側。
    fn thumb(&self, s: f32) -> (i32, i32) {
        let inset = px(4.0, s);
        let track = self.vis - 2 * inset;
        let len = px(32.0, s)
            .max((track as f32 * self.vis as f32 / self.content_h.max(1) as f32).round() as i32)
            .min(track);
        let top = inset
            + if self.max > 0 {
                ((track - len) as f32 * self.scroll as f32 / self.max as f32).round() as i32
            } else {
                0
            };
        (top, len)
    }
}

fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}

fn field(st: &State, i: usize) -> HotkeyField {
    let set = crate::settings();
    let text = crate::hotkey_texts(&set)[i].clone();
    match (&st.recording, &st.error) {
        (Some((r, rec)), _) if *r == i => HotkeyField::Recording(
            Recorder::caps(rec.mods)
                .into_iter()
                .map(String::from)
                .collect(),
        ),
        (_, Some((e, _))) if *e == i => HotkeyField::Failed(
            hotkey::parse(&text)
                .ok()
                .flatten()
                .map(hotkey::caps_of)
                .unwrap_or_default(),
        ),
        _ => HotkeyField::Idle(text),
    }
}

/// トグルの (ログに出す設定名, 設定値への口)。トグルでなければ None。
/// トグルを足すときはここに名指しで足す (`_` は None だけ: 足し忘れは別の値を描かず、トグルにならない)。
type Field = fn(&mut crate::settings::Settings) -> &mut bool;
fn toggle(ctl: Ctl) -> Option<(&'static str, Field)> {
    Some(match ctl {
        Ctl::Takeover => ("printscreen_takeover", |s| &mut s.printscreen_takeover),
        Ctl::Autostart => ("autostart", |s| &mut s.autostart),
        Ctl::ExplorerMenu => ("explorer_menu", |s| &mut s.explorer_menu),
        Ctl::CursorStill => ("cursor_in_still", |s| &mut s.cursor_in_still),
        Ctl::ShowInCapture => ("show_in_capture", |s| &mut s.show_in_capture),
        Ctl::CursorVideo => ("cursor_in_video", |s| &mut s.cursor_in_video),
        Ctl::UpdateCheck => ("update_check", |s| &mut s.update_check),
        _ => return None,
    })
}

/// トグルの表示の値。ストア版の自動起動は設定ではなく `StartupTask` の状態。
fn toggle_value(st: &State, ctl: Ctl) -> bool {
    if ctl == Ctl::Autostart && crate::is_store() {
        return st.startup_on;
    }
    toggle(ctl).is_some_and(|(_, field)| *field(&mut crate::settings()))
}

/// ボタン行の (状態の字, ボタンの字)。ボタンの字が None なら押せない (更新の行だけ)。
fn button_row(st: &State, ctl: Ctl) -> (String, Option<&'static str>) {
    let t = crate::strings();
    match ctl {
        Ctl::SaveDir => (
            path_label(&crate::settings().save_dir, st.s),
            Some(t.change),
        ),
        Ctl::Screenclip => (clip_state(st).into(), Some(t.open_settings)),
        _ => update_state(),
    }
}

/// カウントダウンの表示: 0 は「オフ」、ほかは秒。
fn countdown_text(v: u32) -> String {
    let t = crate::strings();
    if v == 0 {
        t.countdown_off.to_string()
    } else {
        t.seconds.replace("{n}", &v.to_string())
    }
}

/// 録画の上限の表示: 60 の倍数は分、ほかは秒。
fn limit_text(v: u32) -> String {
    let t = crate::strings();
    if v >= 60 && v.is_multiple_of(60) {
        t.minutes.replace("{n}", &(v / 60).to_string())
    } else {
        t.seconds.replace("{n}", &v.to_string())
    }
}

fn layout(st: &State) -> Lay {
    let s = st.s;
    let t = crate::strings();
    let set = crate::settings();
    let cw = st.surface.w as i32;
    let nav_pad = px(16.0, s);
    let row_h = px(32.0, s);
    let nav = std::array::from_fn(|i| {
        let top = nav_pad + i as i32 * (row_h + px(2.0, s));
        rect(nav_pad, top, px(176.0, s) - nav_pad, top + row_h)
    });
    let pane_l = px(176.0, s);
    let gap = px(32.0, s);
    let content_l = pane_l + gap;
    let content_r = cw - gap;
    // 内容の座標 (スクロール 0 で右ペインの上端から) で並べ、最後にスクロールを引く。
    let mut top = gap;
    let mut out = Vec::new();
    let mut labels = Vec::with_capacity(4);
    let mut hint_top = 0;
    // ホットキーの欄の幅は記録中も変えない: 2 つの値と記録中の案内の最も広いものに揃える。
    // ponytail: 修飾キーを 4 つ押したキーキャップは欄からはみ出しうる。
    let hk_w = crate::hotkey_texts(&set)
        .iter()
        .map(|v| HotkeyField::Idle(v.to_string()))
        .chain([HotkeyField::Recording(Vec::new())])
        .map(|f| widgets::hotkey_field_width(&f, t.unassigned, t.press_key, s))
        .max()
        .unwrap_or(0);
    let rows = rows(st);
    for (k, &(ctl, sec)) in rows.iter().enumerate() {
        let first = k == 0 || rows[k - 1].1 != sec;
        if first {
            // 2 つ目からは前のセクションの最後の行の下端から 32 下。ラベル 16、行まで 8。
            if k > 0 {
                top += gap;
            }
            labels.push((ui::text(t.nav[sec], Style::CaptionStrong, s), top));
            top += px(16.0, s) + px(8.0, s);
            // ホットキーはラベルの下にヒントの段を常に取っておく (記録中・失敗で行の高さを変えない)。
            // 高さは長い方の文 (エラー) を今の幅で折り返した高さ。
            if sec == 3 {
                hint_top = top - px(4.0, s);
                let max_w = (content_r - content_l) as f32;
                let err = t
                    .hotkey_in_use
                    .replace("{key}", "Ctrl+Shift+Alt+PrintScreen");
                let note = t.rec_needs_modifier.replace("{key}", "Backspace");
                let h = [
                    (t.rec_hint, false),
                    (err.as_str(), true),
                    (note.as_str(), true),
                    (t.rec_unsupported, true),
                ]
                .iter()
                .map(|&(m, e)| {
                    ui::text_wrapped(m, Style::Caption, s, widgets::hint_text_w(max_w, e, s))
                        .h
                        .ceil() as i32
                })
                .max()
                .unwrap_or(0)
                .max(px(16.0, s));
                top = hint_top + h + px(8.0, s);
            }
        }
        // 部品の幅と高さ。
        let (cw_, ch, hit_extra) = match ctl {
            Ctl::Hotkey(_) => (hk_w, row_h, 0),
            _ if toggle(ctl).is_some() => {
                let (w, h) = widgets::toggle_size(s);
                (w, h, 0)
            }
            Ctl::Language | Ctl::Fps | Ctl::Limit | Ctl::Countdown => (px(144.0, s), row_h, 0),
            // ボタン行 (保存先・ms-screenclip・更新)。ボタンが無いときは幅 0 (押せず、Tab でも止まらない)。
            // 状態の字は部品の左に置く。保存先の字は 180 まで (長いパスは path_label が詰める)。
            _ => {
                let (state, button) = button_row(st, ctl);
                let state = ui::text(&state, Style::Body, s).w.ceil() as i32;
                let state = if ctl == Ctl::SaveDir {
                    state.min(px(180.0, s))
                } else {
                    state
                };
                let w = button.map_or(0, |b| widgets::text_button_width(b, s));
                (w, row_h, state + if w > 0 { px(12.0, s) } else { 0 })
            }
        };
        let label_w = (content_r - cw_ - hit_extra - px(12.0, s) - content_l).max(1);
        let current = t.update_current.replace("{v}", env!("CARGO_PKG_VERSION"));
        let (label, desc) = match ctl {
            Ctl::Hotkey(0) => (t.hotkey_still, None),
            Ctl::Hotkey(_) => (t.hotkey_video, None),
            Ctl::Takeover => (t.printscreen_takeover, None),
            Ctl::Autostart => (t.autostart, None),
            Ctl::ExplorerMenu => (t.explorer_menu, None),
            Ctl::Language => (t.language, None),
            Ctl::CursorStill => (t.cursor_in_still, None),
            Ctl::ShowInCapture => (t.show_in_capture, Some(t.show_in_capture_desc)),
            Ctl::Fps => (t.record_fps, None),
            Ctl::Limit => (t.record_limit, None),
            Ctl::CursorVideo => (t.cursor_in_video, None),
            Ctl::Countdown => (t.record_countdown, None),
            Ctl::SaveDir => (t.save_dir, Some(t.save_dir_desc)),
            Ctl::Screenclip => (t.screenclip, Some(t.screenclip_desc)),
            Ctl::UpdateCheck => (t.update_check, None),
            Ctl::Update => (t.update, Some(current.as_str())),
        };
        let label = ui::text(label, Style::Body, s);
        let desc = desc.map(|d| ui::text_wrapped(d, Style::Caption, s, label_w as f32));
        let lab_h = label.h as i32 + desc.as_ref().map_or(0, |d| d.h as i32);
        let band = lab_h.max(row_h);
        let pad = px(10.0, s);
        let h = (band + 2 * pad).max(px(52.0, s));
        let band_top = top + pad + ((band + 2 * pad).max(px(52.0, s)) - 2 * pad - band) / 2;
        let cy = band_top + (band - ch) / 2;
        let ctrl = rect(content_r - cw_, cy, content_r, cy + ch);
        let hit = rect(
            ctrl.left - hit_extra,
            cy.min(band_top),
            content_r,
            (cy + ch).max(band_top + band),
        );
        let hit = if matches!(ctl, Ctl::SaveDir | Ctl::Screenclip | Ctl::Update) {
            ctrl
        } else {
            hit
        };
        out.push(RowLay {
            ctl,
            sec,
            first,
            top,
            bottom: top + h,
            band_top,
            band,
            ctrl,
            hit,
            label,
            desc,
        });
        top += h;
    }
    let content_h = top + gap;
    let vis = st.surface.h as i32;
    let max = (content_h - vis).max(0);
    let scroll = st.scroll.clamp(0, max);
    let anchors = std::array::from_fn(|i| labels.get(i).map_or(0, |l| l.1 - gap));
    let shift = |r: RECT| rect(r.left, r.top - scroll, r.right, r.bottom - scroll);
    for r in &mut out {
        r.top -= scroll;
        r.bottom -= scroll;
        r.band_top -= scroll;
        r.ctrl = shift(r.ctrl);
        r.hit = shift(r.hit);
    }
    for l in &mut labels {
        l.1 -= scroll;
    }
    Lay {
        nav,
        pane_l,
        content_l,
        content_r,
        rows: out,
        labels,
        hint_top: hint_top - scroll,
        anchors,
        content_h,
        vis,
        max,
        scroll,
    }
}

/// ナビ以外の手段 (ホイール・PageUp / PageDown・つまみ・通り道・Tab) でスクロールの量を `y` にする。
/// 量が変わったら、ナビで押したセクションの記憶を消す (印はスクロールの量から決まる)。
fn scroll_to(st: &mut State, y: i32) {
    if move_to(st, y) {
        st.picked = None;
    }
    log_mark(st);
}

/// スクロールの量を `y` にする (0..=最大値 に収める)。量が変わったら true。`scroll_to`・ナビ・`tick` が通る。
/// 開いている一覧は部品に付いて動かす (部品が右ペインの外へ出たら閉じる)。
/// 滑らかなスクロールは止める (`tick` は呼んだ後で戻す)。
fn move_to(st: &mut State, y: i32) -> bool {
    st.glide = None;
    let old = st.scroll;
    st.scroll = y;
    let lay = layout(st);
    st.scroll = lay.scroll;
    if st.scroll != old {
        st.hover = None;
        log_scroll(&lay, st.s);
        if let Some(ctl) = st.list_open
            && !place_list(st, ctl, Some(st.list_up))
        {
            close_list(st);
        }
    }
    st.scroll != old
}

/// 印のセクション (`mark_of`) が変わったら 1 回だけログに出す。
fn log_mark(st: &mut State) {
    let sec = mark_of(st, &layout(st));
    if sec != st.spied {
        st.spied = sec;
        eprintln!("settings window: section {sec}");
    }
}

/// 実機検証用: スクロールの量・最大値・内容の高さ・つまみ (右ペインの上から)。
fn log_scroll(lay: &Lay, s: f32) {
    let (top, len) = lay.thumb(s);
    eprintln!(
        "settings window: scroll {} / {} (content {}, view {}, thumb {top} +{len}, anchors {:?})",
        lay.scroll, lay.max, lay.content_h, lay.vis, lay.anchors
    );
}

/// 行 k にフォーカスを移したら、右ペインの上下 24 の内側に入れる。セクションの最初の行はラベルの上端から。
fn reveal(st: &mut State, k: usize) {
    let lay = layout(st);
    let Some(r) = lay.rows.get(k) else { return };
    let top = if r.first { lay.labels[r.sec].1 } else { r.top };
    let dy = into_view(top, r.bottom, lay.vis, px(24.0, st.s));
    if dy != 0 {
        scroll_to(st, lay.scroll + dy);
    }
}

/// 保存先の表示: 未設定は「ピクチャ (既定)」。長いパスは途中を「…」で略す。
fn path_label(dir: &Option<PathBuf>, s: f32) -> String {
    match dir {
        None => crate::strings().pictures_default.to_string(),
        Some(p) => {
            let max = px(180.0, s) as f32;
            elide_path(&p.to_string_lossy(), |c| {
                ui::text(c, Style::Body, s).w <= max
            })
        }
    }
}

/// パスの途中のフォルダを「…」にまとめて `fits` に収める。先頭 (ドライブ) と最後のフォルダ名は残す。
/// 残す先頭側のフォルダを後ろから 1 つずつ減らす。どれも収まらなければ `D:\…\最後` を返す。
pub fn elide_path(path: &str, fits: impl Fn(&str) -> bool) -> String {
    if fits(path) {
        return path.to_string();
    }
    let parts: Vec<&str> = path.trim_end_matches('\\').split('\\').collect();
    if parts.len() <= 2 {
        return path.to_string();
    }
    let last = parts[parts.len() - 1];
    let mut cand = String::new();
    for keep in (1..parts.len() - 1).rev() {
        cand = format!("{}\\…\\{last}", parts[..keep].join("\\"));
        if fits(&cand) {
            break;
        }
    }
    cand
}

// ---- 描画 ----

fn paint(st: &State) {
    crate::insight_scope!("settings_paint");
    let pal = ui::palette();
    let s = st.s;
    let t = crate::strings();
    let lay = layout(st);
    // acrylic はアクティブの間だけ DWM が描く。その上に `surface-acrylic` を塗る (acrylic だけでは背後が白いと地が明るく、
    // 白い字が 3.1:1)。非アクティブ (と backdrop の無い OS) は不透明に塗る。
    let opaque = !(st.active && st.backdrop);
    let (w, h) = (st.surface.w as f32, st.surface.h as f32);
    let mark = mark_of(st, &lay);
    let hv = |h: Hot, on: bool| {
        ease(
            st,
            Ak::Hover(h),
            if on { 1.0 } else { 0.0 },
            HOVER_MS,
            Curve::Out,
            0.0,
        )
    };
    let focus_rect = match st.focus {
        Focus::Nav => Some((lay.nav[mark], px(4.0, s) as f32)),
        Focus::Row(k) => lay.rows.get(k).map(|r| {
            let radius = if toggle(r.ctl).is_some() {
                (r.ctrl.bottom - r.ctrl.top) as f32 / 2.0
            } else {
                px(4.0, s) as f32
            };
            (r.ctrl, radius)
        }),
    };
    st.surface.draw(|ctx| {
        let ground = if opaque {
            pal.surface_opaque
        } else {
            pal.surface_acrylic
        };
        fill_rect(ctx, rectf(0.0, 0.0, w, h), ground);
        for (i, r) in lay.nav.iter().enumerate() {
            widgets::nav_row(
                ctx,
                rf(*r),
                NAV_ICONS[i],
                t.nav[i],
                i == mark,
                hv(Hot::Nav(i), st.hover == Some(Hot::Nav(i))),
                pal,
                s,
            );
        }
        // 印は選んだ行へ滑らせる。
        let m = ease(st, Ak::Mark, mark as f32, MARK_MS, Curve::Out, mark as f32);
        let pitch = (lay.nav[1].top - lay.nav[0].top) as f32;
        widgets::nav_marker(
            ctx,
            lay.nav[0].left as f32,
            lay.nav[0].top as f32 + m * pitch,
            pal,
            s,
        );
        // 右ペインの外 (ナビの上) には描かない。
        unsafe {
            ctx.PushAxisAlignedClip(
                &rectf(lay.pane_l as f32, 0.0, w, h),
                D2D1_ANTIALIAS_MODE_ALIASED,
            );
        }
        // 選んだ文字の地。文字より先に塗る。
        if let Some((item, a, b)) = st.sel
            && let Some((t, x, y)) = lay.text(item)
        {
            let wash = windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
                a: 0.22,
                ..pal.focus_ring
            };
            for r in t.range_rects(a.min(b), a.max(b), x, y) {
                fill_rect(ctx, r, wash);
            }
        }
        for (label, top) in &lay.labels {
            draw_text(
                ctx,
                label,
                lay.content_l as f32,
                *top as f32,
                pal.text_secondary,
            );
        }
        // ホットキーのヒントの段: 失敗したらエラー文、ほかは記録の操作の案内。
        let hint_w = (lay.content_r - lay.content_l) as f32;
        let (msg, error) = match (&st.error, &st.rec_note) {
            (Some((_, m)), _) | (None, Some(m)) => (m.as_str(), true),
            (None, None) => (t.rec_hint, false),
        };
        widgets::hint(
            ctx,
            lay.content_l as f32,
            lay.hint_top as f32,
            hint_w,
            msg,
            error,
            pal,
            s,
        );
        for (k, row) in lay.rows.iter().enumerate() {
            // セクションの 2 行目からは上辺の内側に 1px。ラベルのすぐ下の行とセクションの間には引かない。
            if !row.first {
                fill_rect(
                    ctx,
                    rectf(
                        lay.content_l as f32,
                        row.top as f32,
                        lay.content_r as f32,
                        (row.top + stroke(1.0, s)) as f32,
                    ),
                    pal.hairline,
                );
            }
            let ly = row.label_y();
            draw_text(ctx, &row.label, lay.content_l as f32, ly, pal.text_primary);
            if let Some(d) = &row.desc {
                draw_text(
                    ctx,
                    d,
                    lay.content_l as f32,
                    ly + row.label.h,
                    pal.text_secondary,
                );
            }
            let hovered = st.hover == Some(Hot::Row(k));
            match row.ctl {
                Ctl::Hotkey(i) => {
                    widgets::hotkey_field(
                        ctx,
                        rf(row.ctrl),
                        &field(st, i),
                        t.unassigned,
                        t.press_key,
                        pal,
                        s,
                    );
                }
                _ if toggle(row.ctl).is_some() => {
                    let on = if toggle_value(st, row.ctl) { 1.0 } else { 0.0 };
                    let pos = ease(st, Ak::Toggle(k), on, TOGGLE_MS, Curve::Back, on);
                    widgets::toggle(
                        ctx,
                        row.ctrl.left as f32,
                        row.ctrl.top as f32,
                        pos,
                        hv(Hot::Row(k), hovered),
                        pal,
                        s,
                    );
                }
                Ctl::Language | Ctl::Fps | Ctl::Limit | Ctl::Countdown => {
                    let value = dropdown_value(row.ctl);
                    widgets::dropdown(
                        ctx,
                        rf(row.ctrl),
                        &value,
                        hv(Hot::Row(k), hovered || st.list_open == Some(row.ctl)),
                        pal,
                        s,
                    );
                }
                // ボタン行 (保存先・ms-screenclip・更新)。
                _ => {
                    let (state, button) = button_row(st, row.ctl);
                    let p = ui::text(&state, Style::Body, s);
                    let cy = ((row.ctrl.top + row.ctrl.bottom) as f32 - p.h) / 2.0;
                    let gap = if button.is_some() { px(12.0, s) } else { 0 };
                    draw_text(
                        ctx,
                        &p,
                        (row.ctrl.left - gap) as f32 - p.w,
                        cy.round(),
                        pal.text_secondary,
                    );
                    if let Some(b) = button {
                        widgets::text_button(
                            ctx,
                            rf(row.ctrl),
                            b,
                            hv(Hot::Row(k), hovered),
                            pal,
                            s,
                        );
                    }
                }
            }
        }
        // スクロールバー: 右辺から 4 内側、太さ 4 の丸いつまみ (レールなし)。
        // 太さは変えず、ホバーで `text-secondary` へ遷移、ドラッグは `text-primary`。
        if lay.max > 0 {
            let (top, len) = lay.thumb(s);
            let r = w - px(4.0, s) as f32;
            let bw = px(4.0, s) as f32;
            let color = if st.drag.is_some() {
                pal.text_primary
            } else {
                widgets::mix(
                    pal.border_strong,
                    pal.text_secondary,
                    hv(Hot::Bar, st.hover == Some(Hot::Bar)),
                )
            };
            fill_round(
                ctx,
                rectf(r - bw, top as f32, r, (top + len) as f32),
                bw / 2.0,
                color,
            );
        }
        unsafe {
            ctx.PopAxisAlignedClip();
        }
        if st.focus_visible
            && st.recording.is_none()
            && let Some((r, radius)) = focus_rect
        {
            widgets::focus_ring(ctx, rf(r), radius, pal, s);
        }
    });
}

fn dropdown_value(ctl: Ctl) -> String {
    let set = crate::settings();
    match ctl {
        Ctl::Language => crate::strings().language_items[lang_index(&set.language)].to_string(),
        Ctl::Limit => limit_text(set.record_limit_secs),
        Ctl::Countdown => countdown_text(set.record_countdown_secs),
        _ => format!("{} fps", set.record_fps),
    }
}

fn lang_index(lang: &str) -> usize {
    LANGS.iter().position(|l| *l == lang).unwrap_or(0)
}

fn list_labels(ctl: Ctl) -> Vec<String> {
    match ctl {
        Ctl::Language => crate::strings()
            .language_items
            .iter()
            .map(|s| s.to_string())
            .collect(),
        Ctl::Limit => LIMITS.iter().map(|&v| limit_text(v)).collect(),
        Ctl::Countdown => COUNTDOWNS.iter().map(|&v| countdown_text(v)).collect(),
        _ => FPS.iter().map(|f| format!("{f} fps")).collect(),
    }
}

/// 一覧で印を付ける行。今の値が一覧に無い (手で書き換えた toml・前の版のステッパーの値) なら None。
fn list_sel(ctl: Ctl) -> Option<usize> {
    let set = crate::settings();
    match ctl {
        Ctl::Language => Some(lang_index(&set.language)),
        Ctl::Limit => LIMITS.iter().position(|&v| v == set.record_limit_secs),
        Ctl::Countdown => COUNTDOWNS
            .iter()
            .position(|&v| v == set.record_countdown_secs),
        _ => FPS.iter().position(|&f| f == set.record_fps),
    }
}

fn paint_list(st: &State) {
    let Some(ctl) = st.list_open else { return };
    let pal = ui::palette();
    let labels = list_labels(ctl);
    let w = st.list_rect.right - st.list_rect.left;
    // 開くときは部品の側から 8 滑らせて、透明から出す (閉じるのは即座)。
    let p = ease(st, Ak::List, 1.0, LIST_MS, Curve::Out, 0.0);
    let dir = if st.list_up { 1.0 } else { -1.0 };
    let dy = (1.0 - p) * px(8.0, st.s) as f32 * dir;
    st.list.paint(pal, |ctx| unsafe {
        ctx.SetTransform(&windows_numerics::Matrix3x2::translation(0.0, dy));
        ctx.PushLayer(
            &D2D1_LAYER_PARAMETERS1 {
                contentBounds: rectf(-f32::MAX, -f32::MAX, f32::MAX, f32::MAX),
                maskTransform: ui::identity(),
                opacity: p,
                ..Default::default()
            },
            None,
        );
        widgets::draw_list(ctx, &labels, list_sel(ctl), st.list_hover, w, pal, st.s);
        ctx.PopLayer();
        ctx.SetTransform(&ui::identity());
    });
}

/// 一覧を部品の下 4 に、右端を揃えて出す。下に入らなければ上 4。
fn open_list(st: &mut State, ctl: Ctl) {
    if !place_list(st, ctl, None) {
        return;
    }
    st.list_open = Some(ctl);
    st.tweens.borrow_mut().retain(|k| *k != Ak::List);
    st.list_hover = list_sel(ctl);
    st.list.set_theme(ui::apps_dark());
    st.list.show_at(st.list_rect);
    paint_list(st);
    eprintln!(
        "settings window: list {:?} at {:?}",
        ctl,
        (st.list_rect.left, st.list_rect.top)
    );
}

/// 一覧の外形 (`list_rect`) を部品に合わせる。`up` を渡したらその向きに出す (スクロールで付いて動く間は向きを変えない)。
/// 部品が右ペインの見える範囲に無ければ false。開いている一覧はその場で動かす。
fn place_list(st: &mut State, ctl: Ctl, up: Option<bool>) -> bool {
    let lay = layout(st);
    let Some(row) = lay.rows.iter().find(|r| r.ctl == ctl) else {
        return false;
    };
    if row.ctrl.bottom <= 0 || row.ctrl.top >= lay.vis {
        return false;
    }
    let s = st.s;
    let n = list_labels(ctl).len();
    let (lw, lh) = widgets::list_size(n, row.ctrl.right - row.ctrl.left, s);
    let mut tl = POINT {
        x: row.ctrl.left,
        y: row.ctrl.top,
    };
    let mut br = POINT {
        x: row.ctrl.right,
        y: row.ctrl.bottom,
    };
    unsafe {
        let _ = ClientToScreen(st.hwnd, &mut tl);
        let _ = ClientToScreen(st.hwnd, &mut br);
    }
    let below = br.y + px(4.0, s);
    let up = up.unwrap_or(below + lh > ui::monitor_at(tl).work.bottom);
    let y = if up { tl.y - px(4.0, s) - lh } else { below };
    st.list_rect = rect(br.x - lw, y, br.x, y + lh);
    st.list_up = up;
    if st.list_open.is_some() {
        st.list.show_at(st.list_rect);
        paint_list(st);
    }
    true
}

fn close_list(st: &mut State) {
    if st.list_open.take().is_some() {
        st.list.hide();
        st.list_hover = None;
    }
}

// ---- 操作 ----

/// 部品の動作。フォルダの選択だけはモーダルのループを回すので、状態を借りずに行う (呼び手が `PickFolder` を受けて実行)。
enum After {
    None,
    PickFolder,
    Retitle,
}

fn activate(st: &mut State, ctl: Ctl) -> After {
    match ctl {
        Ctl::Hotkey(i) => {
            start_recording(st, i);
        }
        Ctl::Takeover => {
            let on = !toggle_value(st, ctl);
            if let Err(e) = crate::set_takeover(on) {
                eprintln!("takeover: failed: {e}");
            }
            eprintln!("settings: printscreen_takeover {on}");
        }
        Ctl::Language | Ctl::Fps | Ctl::Limit | Ctl::Countdown => {
            if st.list_open == Some(ctl) {
                close_list(st);
            } else {
                close_list(st);
                open_list(st, ctl);
            }
        }
        Ctl::SaveDir => return After::PickFolder,
        Ctl::Screenclip => {
            // 既定のアプリの Manent のページ (MS-SCREENCLIP の行) を直接開く (2026-11-08 RTX 4070 の机)。
            let uri = format!(
                "ms-settings:defaultapps?registeredAppUser={}",
                crate::PRODUCT_NAME
            );
            let r = unsafe {
                ShellExecuteW(
                    Some(st.hwnd),
                    w!("open"),
                    &HSTRING::from(uri),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    SW_SHOWNORMAL,
                )
            };
            eprintln!("settings: open default apps ({})", r.0 as isize);
        }
        Ctl::Update => {
            let crate::update::Status::Available(release) = crate::update::status() else {
                return After::None;
            };
            if release.installer.is_some() && crate::update::installed() {
                crate::update::set_status(crate::update::Status::Downloading(release.clone()));
                crate::update::download_async(release);
            } else {
                let r = unsafe {
                    ShellExecuteW(
                        Some(st.hwnd),
                        w!("open"),
                        &HSTRING::from(release.html_url.as_str()),
                        PCWSTR::null(),
                        PCWSTR::null(),
                        SW_SHOWNORMAL,
                    )
                };
                eprintln!("update: open release page ({})", r.0 as isize);
            }
        }
        // 横取り以外のトグル: 反転して保存し、ログを出してから、トグルごとの副作用。
        _ => {
            let Some((name, field)) = toggle(ctl) else {
                return After::None;
            };
            let on = !toggle_value(st, ctl);
            crate::update_settings(|s| *field(s) = on);
            if ctl == Ctl::Autostart
                && let Err(e) = crate::set_autostart(on)
            {
                eprintln!("autostart: failed: {e}");
            } else {
                eprintln!("settings: {name} {on}");
            }
            if ctl == Ctl::Autostart {
                st.startup_on = startup_on();
            }
            match ctl {
                Ctl::ExplorerMenu => crate::apply_explorer_menu(),
                Ctl::ShowInCapture => ui::set_show_in_capture(on),
                Ctl::UpdateCheck if on => {
                    crate::update::set_status(crate::update::Status::Unknown);
                    crate::check_update();
                }
                _ => {}
            }
        }
    }
    After::None
}

/// 更新の行の状態の字と、ボタンの字 (無ければ押せない)。
/// 結果が来る前と失敗は「最新版です」と出さない。
fn update_state() -> (String, Option<&'static str>) {
    use crate::update::Status;
    let t = crate::strings();
    if !crate::settings().update_check {
        return (t.update_off.into(), None);
    }
    match crate::update::status() {
        Status::Unknown => (t.update_unknown.into(), None),
        Status::Latest => (t.update_latest.into(), None),
        Status::Failed => (t.update_failed.into(), None),
        Status::Downloading(_) => (t.update_downloading.into(), None),
        Status::Available(r) => {
            let button = if r.installer.is_some() && crate::update::installed() {
                t.update_install
            } else {
                t.update_open_page
            };
            (t.update_new.replace("{v}", &r.version), Some(button))
        }
    }
}

/// 更新の確認の結果が変わった。開いていれば描き直す。
pub fn update_changed() {
    with(|st| paint(st));
}

/// ms-screenclip の行の状態の文字。
fn clip_state(st: &State) -> &'static str {
    let t = crate::strings();
    if st.clip_ours {
        t.screenclip_on
    } else {
        t.screenclip_off
    }
}

/// 一覧の行 i を選ぶ。
fn choose(st: &mut State, i: usize) -> After {
    let Some(ctl) = st.list_open else {
        return After::None;
    };
    close_list(st);
    match ctl {
        Ctl::Language => {
            crate::set_language(LANGS[i]);
            After::Retitle
        }
        Ctl::Limit => {
            let v = LIMITS[i];
            crate::update_settings(|s| s.record_limit_secs = v);
            eprintln!("settings: record_limit_secs {v}");
            After::None
        }
        Ctl::Countdown => {
            let v = COUNTDOWNS[i];
            crate::update_settings(|s| s.record_countdown_secs = v);
            eprintln!("settings: record_countdown_secs {v}");
            After::None
        }
        _ => {
            let fps = FPS[i];
            crate::update_settings(|s| s.record_fps = fps);
            eprintln!("settings: record_fps {fps}");
            After::None
        }
    }
}

fn start_recording(st: &mut State, i: usize) {
    close_list(st);
    st.error = None;
    st.recording = Some((i, Recorder::default()));
    // 記録中は自分のホットキーを外し、押したキーが窓に届くようにする。PrintScreen はフックが窓へ送る。
    crate::unregister_hotkeys();
    rec_hook(Some(st.hwnd));
    eprintln!("settings: recording {:?}", crate::ACTIONS[i]);
}

fn stop_recording(st: &mut State) -> Option<usize> {
    let (i, _) = st.recording.take()?;
    st.rec_note = None;
    rec_hook(None);
    Some(i)
}

fn cancel_recording(st: &mut State) {
    // 確定したキーを押したまま記録が終わる (欄の外を押す・窓が非アクティブになる) なら、取り消さずに確定する。
    if st.held.is_some() {
        finish_recording(st);
        return;
    }
    if stop_recording(st).is_some() {
        let _ = crate::reregister_hotkeys();
        eprintln!("settings: recording cancelled");
    }
}

/// 記録中のキー。確定・取り消し・未割当てで記録を終える。
fn record_key(st: &mut State, vk: u32, down: bool) {
    let Some((i, rec)) = st.recording.as_mut() else {
        return;
    };
    if let Some((held_vk, _)) = st.held {
        // 確定のキーを離すまで、自動の繰り返しも他のキーも飲む。
        if !down && vk == held_vk {
            finish_recording(st);
        }
        return;
    }
    match rec.key(vk, down) {
        None => {}
        Some(Recorded::Cancel) => cancel_recording(st),
        Some(Recorded::NeedsModifier(k)) => {
            let key = hotkey::format(Some(hotkey::Hotkey { mods: 0, vk: k }));
            st.rec_note = Some(crate::strings().rec_needs_modifier.replace("{key}", &key));
        }
        Some(Recorded::Unsupported) => {
            st.rec_note = Some(crate::strings().rec_unsupported.to_string());
        }
        Some(out) => {
            let text = match out {
                Recorded::Key(hk) => hotkey::format(Some(hk)),
                Recorded::Default => {
                    crate::hotkey_texts(&crate::settings::Settings::default())[*i].clone()
                }
                _ => String::new(),
            };
            st.held = Some((vk, text));
        }
    }
}

/// 確定したキーが離されたので、記録を終えて登録する。
fn finish_recording(st: &mut State) {
    let Some((_, text)) = st.held.take() else {
        return;
    };
    let Some(i) = stop_recording(st) else {
        return;
    };
    match crate::set_hotkey(i, text.clone()) {
        Ok(()) => eprintln!("settings: hotkey {:?} = {text:?}", crate::ACTIONS[i]),
        Err(e) => {
            eprintln!(
                "settings: hotkey {:?} = {text} failed: {e}",
                crate::ACTIONS[i]
            );
            st.error = Some((i, e));
        }
    }
}

fn pick_folder(hwnd: HWND, current: Option<PathBuf>) -> windows::core::Result<Option<PathBuf>> {
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
        dialog.SetOptions(dialog.GetOptions()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)?;
        let folder: Option<IShellItem> = match &current {
            Some(d) => SHCreateItemFromParsingName(&HSTRING::from(d.as_os_str()), None).ok(),
            None => SHGetKnownFolderItem(&FOLDERID_Pictures, KF_FLAG_DEFAULT, None).ok(),
        };
        if let Some(f) = folder {
            let _ = dialog.SetFolder(&f);
        }
        if dialog.Show(Some(hwnd)).is_err() {
            return Ok(None);
        }
        let item = dialog.GetResult()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH)?;
        let path = PathBuf::from(name.to_string()?);
        CoTaskMemFree(Some(name.0 as _));
        Ok(Some(path))
    }
}

fn after(hwnd: HWND, a: After) {
    match a {
        After::None => {}
        After::Retitle => unsafe {
            let _ = SetWindowTextW(hwnd, &HSTRING::from(crate::strings().settings_title));
        },
        After::PickFolder => match pick_folder(hwnd, crate::settings().save_dir) {
            Ok(Some(dir)) => {
                eprintln!("settings: save_dir {}", dir.display());
                crate::update_settings(|s| s.save_dir = Some(dir));
            }
            Ok(None) => {}
            Err(e) => eprintln!("settings: folder dialog failed: {e}"),
        },
    }
    with(|st| paint(st));
}

fn hit(st: &State, x: i32, y: i32) -> Option<Hot> {
    let lay = layout(st);
    if let Some(i) = lay.nav.iter().position(|r| contains(r, POINT { x, y })) {
        return Some(Hot::Nav(i));
    }
    // スクロールバーの当たりは右端 16 (見た目の 8 より広く取る)。
    if lay.max > 0 && x >= st.surface.w as i32 - px(16.0, st.s) {
        return Some(Hot::Bar);
    }
    lay.rows
        .iter()
        .position(|r| contains(&r.hit, POINT { x, y }))
        .map(Hot::Row)
}

fn on_click(st: &mut State, x: i32, y: i32) -> After {
    st.focus_visible = false;
    if st.sel.take().is_some() {
        paint(st);
    }
    // 開いている一覧の部品をもう一度押したら、閉じるだけにする。
    let was_open = st.list_open;
    close_list(st);
    let target = hit(st, x, y);
    // 記録中に欄の外を押したら取り消す。
    if let Some((i, _)) = st.recording {
        let same = match target {
            Some(Hot::Row(k)) => rows(st).get(k).map(|r| r.0) == Some(Ctl::Hotkey(i)),
            _ => false,
        };
        cancel_recording(st);
        if same {
            return After::None;
        }
    }
    match target {
        Some(Hot::Nav(i)) => {
            jump_to(st, i);
            st.focus = Focus::Nav;
            After::None
        }
        Some(Hot::Row(k)) => {
            st.focus = Focus::Row(k);
            let Some(&(ctl, _)) = rows(st).get(k) else {
                return After::None;
            };
            if was_open == Some(ctl) {
                return After::None;
            }
            activate(st, ctl)
        }
        Some(Hot::Bar) => {
            // つまみを押したらドラッグ、レールを押したらその向きへ 1 ページ。
            let lay = layout(st);
            let (top, len) = lay.thumb(st.s);
            if y >= top && y < top + len {
                st.drag = Some(y - top);
                unsafe {
                    SetCapture(st.hwnd);
                }
            } else {
                let page = page(&lay, st.s);
                scroll_to(st, st.scroll + if y < top { -page } else { page });
            }
            After::None
        }
        None => {
            select_at(st, x, y);
            After::None
        }
    }
}

/// 文字の上で押した: 1 回目は選び始め (ドラッグで伸ばす)、ダブルクリックはその文字を全部選ぶ。
fn select_at(st: &mut State, x: i32, y: i32) {
    let lay = layout(st);
    let Some(item) = lay.text_hit(x, y) else {
        st.last_click = None;
        return;
    };
    let Some((t, ox, oy)) = lay.text(item) else {
        return;
    };
    let (dx, dy, ms) = unsafe {
        (
            GetSystemMetrics(SM_CXDOUBLECLK),
            GetSystemMetrics(SM_CYDOUBLECLK),
            GetDoubleClickTime(),
        )
    };
    let double = st.last_click.is_some_and(|(at, lx, ly)| {
        at.elapsed().as_millis() <= ms as u128
            && (x - lx).abs() <= dx / 2
            && (y - ly).abs() <= dy / 2
    });
    if double {
        st.sel = Some((item, 0, t.len()));
        st.last_click = None;
    } else {
        let p = t.pos_at(x as f32 - ox, y as f32 - oy);
        st.sel = Some((item, p, p));
        st.selecting = true;
        st.last_click = Some((Instant::now(), x, y));
        unsafe {
            SetCapture(st.hwnd);
        }
    }
    paint(st);
}

/// ドラッグ中: 選んだ文字の中で範囲の終わりを動かす。
fn select_to(st: &mut State, x: i32, y: i32) {
    let Some((item, a, _)) = st.sel else { return };
    let lay = layout(st);
    let Some((t, ox, oy)) = lay.text(item) else {
        return;
    };
    let p = t.pos_at(x as f32 - ox, y as f32 - oy);
    st.sel = Some((item, a, p));
    paint(st);
}

/// Ctrl+C: 選んだ文字をクリップボードへ。
fn copy_selection(st: &State) {
    let Some((item, a, b)) = st.sel else { return };
    let lay = layout(st);
    let Some((t, _, _)) = lay.text(item) else {
        return;
    };
    let text = t.slice(a.min(b), a.max(b));
    if text.is_empty() {
        return;
    }
    match crate::export::copy_text(st.hwnd, &text) {
        Ok(()) => eprintln!("settings: copied {} chars", text.chars().count()),
        Err(e) => eprintln!("settings: copy failed: {e}"),
    }
}

/// PageUp / PageDown とレールを押したときの量: 見える高さ − 48。
fn page(lay: &Lay, s: f32) -> i32 {
    (lay.vis - px(48.0, s)).max(px(48.0, s))
}

/// ドラッグ中、つまみの上端を `y − grab` に合わせる。
fn drag_to(st: &mut State, y: i32, grab: i32) {
    let lay = layout(st);
    let (_, len) = lay.thumb(st.s);
    let inset = px(4.0, st.s);
    let room = (lay.vis - 2 * inset - len).max(1);
    let v = ((y - grab - inset) as f32 * lay.max as f32 / room as f32).round() as i32;
    scroll_to(st, v);
}

/// ナビのセクション i のアンカーへ滑らかにスクロールする (`tick` が進める。動きを切っていればすぐ)。
/// 印は i に付ける (アンカーが最大値を超えて、スクロールの量からは別のセクションになるときも)。
/// 前の版でページを替えたときと同じく、登録失敗のエラー文を消す。
fn jump_to(st: &mut State, i: usize) {
    st.error = None;
    st.picked = Some(i);
    let lay = layout(st);
    let to = jump(lay.anchors[i], lay.max);
    close_list(st);
    if !st.motion || to == lay.scroll {
        move_to(st, to);
    } else {
        st.glide = Some((lay.scroll, to, Instant::now()));
        st.hover = None;
        run_anim(st);
    }
    log_mark(st);
}

fn on_key(st: &mut State, vk: u16) -> After {
    if vk == u16::from(b'C') && unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0 {
        copy_selection(st);
        return After::None;
    }
    let n = rows(st).len();
    // PageUp / PageDown は見える高さ − 48。一覧を開いていたら閉じてからスクロールする (scroll_to)。
    // 記録中のキーは wndproc が先に記録へ回すので、ここには来ない。
    if vk == VK_PRIOR.0 || vk == VK_NEXT.0 {
        let page = page(&layout(st), st.s);
        let dy = if vk == VK_PRIOR.0 { -page } else { page };
        scroll_to(st, st.scroll + dy);
        return After::None;
    }
    // 一覧を開いている間は一覧を操作する。
    if let Some(ctl) = st.list_open {
        let count = list_labels(ctl).len();
        let cur = st.list_hover.unwrap_or(0);
        match vk {
            v if v == VK_UP.0 => st.list_hover = Some(cur.saturating_sub(1)),
            v if v == VK_DOWN.0 => st.list_hover = Some((cur + 1).min(count - 1)),
            // 今の行が無い (toml の値が一覧に無い) なら選ばずに閉じる。先頭を選ぶと値が変わる。
            v if v == VK_RETURN.0 || v == VK_SPACE.0 => match st.list_hover {
                Some(i) => return choose(st, i),
                None => close_list(st),
            },
            v if v == VK_ESCAPE.0 || v == VK_TAB.0 => close_list(st),
            _ => {}
        }
        paint_list(st);
        return After::None;
    }
    if vk == VK_TAB.0 {
        st.focus_visible = true;
        let back = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
        // 0 はナビ、1.. は行。
        let cur = match st.focus {
            Focus::Nav => 0,
            Focus::Row(k) => k + 1,
        };
        let step = |c: usize| {
            if back {
                (c + n) % (n + 1)
            } else {
                (c + 1) % (n + 1)
            }
        };
        let mut next = step(cur);
        // ボタンの無い更新の行には止まらない (押すものが無い)。
        if next > 0 && rows(st)[next - 1].0 == Ctl::Update && update_state().1.is_none() {
            next = step(next);
        }
        st.focus = if next == 0 {
            Focus::Nav
        } else {
            Focus::Row(next - 1)
        };
        if next > 0 {
            reveal(st, next - 1);
        }
        return After::None;
    }
    let arrow = [VK_UP.0, VK_DOWN.0, VK_LEFT.0, VK_RIGHT.0].contains(&vk);
    let press = vk == VK_SPACE.0 || vk == VK_RETURN.0;
    if !arrow && !press {
        return After::None;
    }
    st.focus_visible = true;
    match st.focus {
        Focus::Nav => {
            // 印を前後のセクションへ動かし、そのアンカーへスクロールする。
            let mark = mark_of(st, &layout(st));
            if vk == VK_UP.0 && mark > 0 {
                jump_to(st, mark - 1);
            } else if vk == VK_DOWN.0 && mark < 3 {
                jump_to(st, mark + 1);
            }
            After::None
        }
        Focus::Row(k) => {
            let Some(&(ctl, _)) = rows(st).get(k) else {
                return After::None;
            };
            match ctl {
                // トグルは Space、ほかは Space か Enter。
                _ if toggle(ctl).is_some() && vk == VK_SPACE.0 => activate(st, ctl),
                Ctl::Hotkey(_)
                | Ctl::Language
                | Ctl::Fps
                | Ctl::Limit
                | Ctl::Countdown
                | Ctl::SaveDir
                | Ctl::Screenclip
                | Ctl::Update
                    if press =>
                {
                    activate(st, ctl)
                }
                _ => After::None,
            }
        }
    }
}

// ---- 記録中のキーボードフック ----

/// 記録中のキーを送る窓。0 なら記録していない。
static REC_HWND: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    static REC_HOOK: std::cell::Cell<Option<HHOOK>> = const { std::cell::Cell::new(None) };
}

/// 記録の間だけ UI スレッドに LL フックを入れ、全部のキーを飲んで窓へ送る。
/// `RegisterHotKey` は窓のキー入力より先にキーを取るので、他のアプリが登録したキーも
/// このフックでなければ窓に届かない (LL フックは登録済みのホットキーの判定より前に呼ばれる)。
/// 後から入れたフックが先に呼ばれるので、横取りフックより先に PrintScreen も取る。
fn rec_hook(target: Option<HWND>) {
    REC_HOOK.with(|h| {
        if let Some(old) = h.take() {
            let _ = unsafe { UnhookWindowsHookEx(old) };
        }
        REC_HWND.store(target.map_or(0, |w| w.0 as isize), Ordering::Relaxed);
        if target.is_some() {
            match unsafe {
                SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(rec_hook_proc),
                    GetModuleHandleW(None).ok().map(Into::into),
                    0,
                )
            } {
                Ok(hook) => h.set(Some(hook)),
                // 入らなければ窓の WM_KEYDOWN で記録する (他のアプリのホットキーは届かない)。
                Err(e) => eprintln!("settings: recording hook failed: {e}"),
            }
        }
    });
}

unsafe extern "system" fn rec_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let target = REC_HWND.load(Ordering::Relaxed);
    if code == HC_ACTION as i32 && target != 0 {
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let _ = unsafe {
            PostMessageW(
                Some(HWND(target as *mut _)),
                crate::WM_REC_KEY,
                WPARAM(kb.vkCode as usize),
                LPARAM(down as isize),
            )
        };
        return LRESULT(1);
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

// ---- 窓のプロシージャ ----

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let def = || unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    match msg {
        WM_PAINT => {
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            with(|st| paint(st));
            LRESULT(0)
        }
        // 一覧は画面座標の別窓なので、窓を動かすと古い位置に残る。動かしたら閉じる (Win+矢印も WM_MOVE が来る)。
        WM_MOVE => {
            with(close_list);
            def()
        }
        // 最小化はクライアント 0×0 で来る。描かない (折り返し幅が負になる)。戻すときに次の WM_SIZE が来る。
        WM_SIZE if wparam.0 as u32 == SIZE_MINIMIZED => LRESULT(0),
        WM_SIZE => {
            let POINT { x: w, y: h } = lparam_point(lparam);
            with(|st| {
                let _ = st.surface.resize(w as u32, h as u32);
                // 大きくすると最大値が減るので、スクロールの量を収め直す。一覧は部品とずれるので閉じる。
                // 収め直して量が変わったら、ナビで押したセクションの記憶を消す。
                close_list(st);
                let y = layout(st).scroll;
                if y != st.scroll {
                    st.scroll = y;
                    st.picked = None;
                }
                log_mark(st);
                paint(st);
            });
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_ANIM => {
            with(tick);
            LRESULT(0)
        }
        // 押せる部品 (ナビ・行の部品) の上では手の形にする。
        WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
            let mut pt = ui::cursor();
            unsafe {
                let _ = ScreenToClient(hwnd, &mut pt);
            }
            // 選べる文字の上では I の形。
            let cursor = with(|st| {
                if st.drag.is_some() {
                    None
                } else if st.selecting {
                    Some(IDC_IBEAM)
                } else {
                    match hit(st, pt.x, pt.y) {
                        Some(Hot::Nav(_) | Hot::Row(_)) => Some(IDC_HAND),
                        Some(Hot::Bar) => None,
                        None => layout(st).text_hit(pt.x, pt.y).map(|_| IDC_IBEAM),
                    }
                }
            })
            .flatten();
            if let Some(c) = cursor {
                unsafe {
                    SetCursor(LoadCursorW(None, c).ok());
                }
                LRESULT(1)
            } else {
                def()
            }
        }
        WM_GETMINMAXINFO => {
            let mmi = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
            mmi.ptMinTrackSize = min_track(hwnd);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            let active = (wparam.0 & 0xFFFF) as u32 != WA_INACTIVE;
            with(|st| {
                st.active = active;
                if active {
                    refresh_clip(st);
                }
                if !active {
                    close_list(st);
                    cancel_recording(st);
                }
                eprintln!("settings window: active {active}");
                paint(st);
            });
            def()
        }
        WM_MOUSEWHEEL => {
            // 1 刻み (120) で 48。120 より小さい量は比例で足す。上へ回すと内容が下がる。
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
            with(|st| {
                let dy = -(px(48.0, st.s) * delta / 120);
                scroll_to(st, st.scroll + dy);
                paint(st);
            });
            LRESULT(0)
        }
        WM_DPICHANGED => {
            with(|st| {
                let s = (wparam.0 & 0xFFFF) as f32 / 96.0;
                st.scroll = (st.scroll as f32 * s / st.s).round() as i32;
                st.s = s;
            });
            // 大きさは変えられるので、OS の勧める外形 (今の大きさを新しい倍率で拡げたもの) に合わせる。
            let r = unsafe { &*(lparam.0 as *const RECT) };
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            log_bounds(hwnd);
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            let topic = if lparam.0 == 0 {
                String::new()
            } else {
                unsafe { PCWSTR(lparam.0 as *const u16).to_string() }.unwrap_or_default()
            };
            if topic == "ImmersiveColorSet" {
                set_dark(hwnd);
                with(|st| {
                    st.list.set_theme(ui::apps_dark());
                    paint(st);
                    paint_list(st);
                });
            }
            def()
        }
        WM_MOUSEMOVE => {
            let POINT { x, y } = lparam_point(lparam);
            track_leave(hwnd);
            with(|st| {
                if let Some(grab) = st.drag {
                    drag_to(st, y, grab);
                    paint(st);
                    return;
                }
                if st.selecting {
                    select_to(st, x, y);
                    return;
                }
                let h = hit(st, x, y);
                if st.hover != h {
                    st.hover = h;
                    paint(st);
                }
            });
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            with(|st| {
                if st.hover.take().is_some() {
                    paint(st);
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let POINT { x, y } = lparam_point(lparam);
            if let Some(a) = with(|st| on_click(st, x, y)) {
                after(hwnd, a);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if with(|st| st.drag.is_some() || st.selecting).unwrap_or(false) {
                unsafe {
                    let _ = ReleaseCapture();
                }
            }
            LRESULT(0)
        }
        // ReleaseCapture のほか、Alt+Tab などで捕まえが外れたときも来る。
        WM_CAPTURECHANGED => {
            with(|st| {
                st.selecting = false;
                if st.drag.take().is_some() {
                    st.hover = None;
                    paint(st);
                }
            });
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
            let vk = wparam.0 as u32;
            let recording = with(|st| st.recording.is_some()).unwrap_or(false);
            if recording {
                // PrintScreen は窓には離しだけが届く (押し下げは来ない)。離しを押し下げと離しの対として読む。
                let as_down = down || vk == hotkey::VK_SNAPSHOT;
                with(|st| {
                    record_key(st, vk, as_down);
                    if !down && vk == hotkey::VK_SNAPSHOT {
                        record_key(st, vk, false);
                    }
                    paint(st);
                });
                return LRESULT(0);
            }
            if !down {
                return def();
            }
            match with(|st| {
                let a = on_key(st, vk as u16);
                paint(st);
                a
            }) {
                Some(a) => {
                    after(hwnd, a);
                    if msg == WM_SYSKEYDOWN {
                        def()
                    } else {
                        LRESULT(0)
                    }
                }
                None => def(),
            }
        }
        crate::WM_REC_KEY => {
            with(|st| {
                record_key(st, wparam.0 as u32, lparam.0 != 0);
                paint(st);
            });
            LRESULT(0)
        }
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let st = STATE.with(|st| st.try_borrow_mut().ok().and_then(|mut g| g.take()));
            if let Some(mut st) = st {
                cancel_recording(&mut st);
                unsafe {
                    let _ = DestroyWindow(st.list.hwnd);
                }
            }
            eprintln!("settings window: closed");
            LRESULT(0)
        }
        _ => def(),
    }
}

unsafe extern "system" fn list_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        // 一覧の行は全部押せるので手の形。
        WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
            unsafe {
                SetCursor(LoadCursorW(None, IDC_HAND).ok());
            }
            LRESULT(1)
        }
        // 一覧の上で回したホイールも設定窓のスクロールにする (一覧は部品に付いて動く)。
        WM_MOUSEWHEEL => {
            if let Some(owner) = hwnd_of() {
                unsafe { SendMessageW(owner, msg, Some(wparam), Some(lparam)) };
            }
            LRESULT(0)
        }
        WM_PAINT => {
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            with(|st| paint_list(st));
            LRESULT(0)
        }
        WM_MOUSEMOVE | WM_MOUSELEAVE | WM_LBUTTONUP => {
            let POINT { x, y } = lparam_point(lparam);
            // 離れの追跡は移動のときだけ入れる。WM_MOUSELEAVE の中で入れ直すと、隠した窓では
            // すぐ次の WM_MOUSELEAVE が投函され続け、UI スレッドの入力が届かなくなる (2026-11-04 実測)。
            if msg == WM_MOUSEMOVE {
                track_leave(hwnd);
            }
            let chosen = with(|st| {
                let ctl = st.list_open?;
                let w = st.list_rect.right - st.list_rect.left;
                let row = if msg == WM_MOUSELEAVE {
                    None
                } else {
                    widgets::list_hit(list_labels(ctl).len(), w, x, y, st.s)
                };
                if msg == WM_LBUTTONUP
                    && let Some(i) = row
                {
                    let a = choose(st, i);
                    paint(st);
                    return Some((st.hwnd, a));
                }
                if st.list_hover != row && row.is_some() {
                    st.list_hover = row;
                    paint_list(st);
                }
                None
            })
            .flatten();
            if let Some((owner, a)) = chosen {
                after(owner, a);
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
    fn scroll_spy_and_limits() {
        // DS SettingsWindow の見本: アンカー 0・192・308・500、内容 816、見える高さ 364、最大値 452。
        let anchors = [0, 192, 308, 500];
        let max = 452;
        assert_eq!(spy(&anchors, 0, max), 0);
        assert_eq!(spy(&anchors, 191, max), 0);
        assert_eq!(spy(&anchors, 192, max), 1);
        assert_eq!(spy(&anchors, 400, max), 2);
        // いちばん下ではホットキー (アンカー 500 は最大値 452 に届かない)。
        assert_eq!(spy(&anchors, 451, max), 2);
        assert_eq!(spy(&anchors, 452, max), 3);
        // ナビのジャンプはいちばん下を超えない。ヒントの段で最大値 472 になっても同じ。
        assert_eq!(jump(500, max), 452);
        assert_eq!(jump(308, max), 308);
        assert_eq!(jump(500, 472), 472);
        assert_eq!(spy(&anchors, 472, 472), 3);
        // Tab のフォーカス: 上下 24 の内側へ最小の差。入っていれば動かさない。
        assert_eq!(into_view(24, 72, 364, 24), 0);
        assert_eq!(into_view(10, 58, 364, 24), -14);
        assert_eq!(into_view(300, 352, 364, 24), 12);
    }

    #[test]
    fn path_elision_keeps_drive_and_last_folder() {
        let fits = |n: usize| move |c: &str| c.chars().count() <= n;
        let p = r"D:\Captures\2026\資料\スクリーンショット\odile";
        // 収まるならそのまま。
        assert_eq!(elide_path(p, fits(100)), p);
        // 先頭側のフォルダを後ろから減らし、ドライブと最後のフォルダ名を残す。
        assert_eq!(elide_path(p, fits(24)), r"D:\Captures\2026\…\odile");
        assert_eq!(elide_path(p, fits(20)), r"D:\Captures\…\odile");
        // どれも収まらなければ最短の形。
        assert_eq!(elide_path(p, fits(5)), r"D:\…\odile");
        // 区切りが 1 つ以下なら略さない。
        assert_eq!(elide_path(r"D:\odile", fits(3)), r"D:\odile");
        // 末尾の区切りは無視する。
        assert_eq!(elide_path(r"D:\a\b\c\", fits(8)), r"D:\a\…\c");
        assert_eq!(elide_path(r"D:\a\b\c\", fits(7)), r"D:\…\c");
    }
}
