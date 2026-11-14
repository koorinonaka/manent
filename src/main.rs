// debug ビルドはコンソールを残し、stderr のログを見えるようにする。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod annot;
mod annot_edit;
mod capture;
mod clip;
mod countdown;
mod export;
mod gifpin;
mod hotkey;
mod i18n;
#[cfg(feature = "insight")]
mod insight;
mod jumplist;
mod layout;
mod mark;
mod open;
mod overlay;
mod pin;
#[cfg(debug_assertions)]
mod probe;
mod record;
mod settings;
mod setwin;
mod timeline;
mod traymenu;
mod ui;
mod uia;
mod update;

/// 囲んだブロックの時間を `name` で測る (`--insight` の実行だけ)。`insight` feature が無ければ何も残さない。
/// ブロックの先頭に置く。ブロックの終わりで計測が終わる。
#[cfg(feature = "insight")]
macro_rules! insight_scope {
    ($name:literal) => {
        let _insight_scope = crate::insight::Scope::new($name);
    };
}

#[cfg(not(feature = "insight"))]
macro_rules! insight_scope {
    ($name:literal) => {};
}
pub(crate) use insight_scope;

/// 自分で測った `Duration` を `name` の区間に足す (`--insight` の実行だけ)。
#[cfg(feature = "insight")]
macro_rules! insight_sample {
    ($name:literal, $elapsed:expr) => {
        crate::insight::add($name, $elapsed);
    };
}

#[cfg(not(feature = "insight"))]
macro_rules! insight_sample {
    ($name:literal, $elapsed:expr) => {
        let _ = &$elapsed;
    };
}
pub(crate) use insight_sample;

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::ApplicationModel::{StartupTask, StartupTaskState};
use windows::Win32::Foundation::{
    E_FAIL, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, GetLastError,
    HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegDeleteTreeW,
    RegGetValueW, RegSetKeyValueW,
};
use windows::Win32::System::Threading::{AttachThreadInput, CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor,
    GetSystemMetricsForDpi, GetThreadDpiAwarenessContext, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
    VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::Shell::{
    ASSOCF_IS_PROTOCOL, ASSOCSTR_PROGID, AssocQueryStringW, NIF_ICON, NIF_INFO, NIF_MESSAGE,
    NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIN_BALLOONUSERCLICK,
    NOTIFYICONDATAW, SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify, Shell_NotifyIconW,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

/// 製品名。%APPDATA% のフォルダ・mutex・Run キーの値名・トレイの表示名はここから作る。
/// exe 名だけは Cargo.toml の `name` が決める。
pub const PRODUCT_NAME: &str = "Manent";

const WM_TRAY: u32 = WM_APP + 1;
/// LL フックからの投函。wparam は `ACTIONS` の添字。
const WM_TAKEOVER: u32 = WM_APP + 2;
/// UIA のスレッドからの答え。lparam は `uia::Answer` の箱 (`uia::take`)。
pub const WM_UIA: u32 = WM_APP + 3;
/// ホットキー記録中の LL フックが飲んだキー。設定窓へ送る。wparam は VK、lparam は押し下げなら 1。
pub const WM_REC_KEY: u32 = WM_APP + 4;
/// 更新の確認のスレッドからの知らせ。lparam は `update::Msg` の箱 (`update::take`)。
pub const WM_UPDATE: u32 = WM_APP + 5;
/// 更新の確認。起動の少し後に 1 回 (サインイン直後はネットワークがまだ繋がっていないことがある)、以後 24 時間ごと。
const TIMER_UPDATE: usize = 3;
const UPDATE_FIRST_MS: u32 = 15_000;
const UPDATE_EVERY_MS: u32 = 24 * 60 * 60 * 1000;
/// 通知を出した版 (同じ版はプロセスの寿命の中で 1 回だけ)。
static UPDATE_NOTIFIED: Mutex<Option<String>> = Mutex::new(None);
/// トレイの左クリックから静止画キャプチャまでのタイマー。
const TIMER_TRAY_CLICK: usize = 1;
const TRAY_CLICK_DELAY_MS: u32 = 150;
/// ms-screenclip で起こされたら、PrintScreen が離されるまでこの間隔で待ってから撮る。
const TIMER_CLIP: usize = 2;
const CLIP_POLL_MS: u32 = 10;
/// 待つのはここまで (押しっぱなしでも撮る)。
const CLIP_WAIT_MAX: std::time::Duration = std::time::Duration::from_secs(1);
static CLIP_T0: Mutex<Option<std::time::Instant>> = Mutex::new(None);
/// ジャンプリストの動画キャプチャ・設定。静止画は `TIMER_TRAY_CLICK` を使う。
/// どれもジャンプリストのメニューが消えてから (`TRAY_CLICK_DELAY_MS`)。凍結すると写る。
const TIMER_JUMP_VIDEO: usize = 4;
const TIMER_JUMP_SETTINGS: usize = 5;

/// ms-screenclip の起動を受けた。押したキーが離れる前に範囲選択が前面を取ると、前面のアプリ
/// (FF14 のスクショ) は離しを受け取れず、次の押下を取りこぼす (2026-11-08 RTX 4070 の机)。離されてから撮る。
/// ponytail: 見るのは PrintScreen だけ。Win+Shift+S は S の離しを待たない。
fn start_clip(hwnd: HWND) {
    *CLIP_T0.lock().expect("clip lock") = Some(std::time::Instant::now());
    unsafe { SetTimer(Some(hwnd), TIMER_CLIP, CLIP_POLL_MS, None) };
}
/// `WM_COPYDATA` の `dwData`: 2 本目のプロセスが `--open` のパス (UTF-16、終端なし) を渡す。
const COPYDATA_OPEN: usize = 0x4F50_454E;
/// `WM_COPYDATA` の `dwData`: 2 本目のプロセスが `ms-screenclip:` で起こされた (中身なし)。
const COPYDATA_CLIP: usize = 0x434C_4950;
/// `WM_COPYDATA` の `dwData`: 2 本目のプロセスがジャンプリストから起こされた。中身は `jumplist::Task` の添字 1 つ。
const COPYDATA_TASK: usize = 0x5441_534B;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Still,
    Video,
}

/// `RegisterHotKey` の id は添字 + 1。
const ACTIONS: [Action; 2] = [Action::Still, Action::Video];

/// ホットキー押下の入口 (トレイのメニューのキャプチャの行も)。
pub(crate) fn dispatch(action: Action) {
    eprintln!("hotkey: {action:?}");
    match action {
        Action::Still => still(),
        Action::Video => video(),
    }
}

/// 録画中なら止める。カウント中なら中止する。ほかは生の画面の上で範囲を選び (凍結しない)、
/// 選んだら録画を始める (`record_countdown_secs` が 0 でなければ、録画枠を出してその秒数を数えてから)。
fn video() {
    if countdown::active() {
        countdown::cancel();
        return;
    }
    if record::active() {
        record::stop();
        return;
    }
    if overlay::active() {
        return;
    }
    overlay::start(
        None,
        Box::new(|sel| {
            let Some(sel) = sel else {
                return;
            };
            let secs = settings().record_countdown_secs;
            if secs == 0 {
                record::start(sel.rect);
                return;
            }
            let Some(p) = record::prepare(sel.rect) else {
                return;
            };
            let (rect, work, s) = (p.rect, p.work, p.s);
            countdown::start(
                secs,
                rect,
                work,
                s,
                Box::new(move |go| {
                    if go {
                        record::start_prepared(p);
                    } else {
                        record::cancel(p);
                    }
                }),
            );
        }),
    );
}

/// 全モニタを凍結してオーバーレイで選び、選んだ位置にピンを出す。
fn still() {
    crate::insight_scope!("still_to_overlay");
    if overlay::active() {
        return;
    }
    let shots = match capture::freeze(settings().cursor_in_still) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("capture failed: {e}");
            notify(tray_hwnd(), strings().capture_failed);
            return;
        }
    };
    overlay::start(
        Some(shots),
        Box::new(|sel| {
            if let Some(sel) = sel {
                let image = capture::crop(&sel.shots, sel.rect);
                if let Err(e) = export::copy(tray_hwnd(), &image) {
                    eprintln!("still: copy failed: {e}");
                }
                pin::create(image, sel.rect);
            }
        }),
    );
}

struct App {
    settings_path: PathBuf,
    /// 設定窓が書き換える。読み手は `settings()` の写しを使う (キャプチャのたびに読む)。
    settings: Mutex<settings::Settings>,
}

/// 文言の言語。設定窓で変えたらすぐ切り替わる。
static LANG_JA: AtomicBool = AtomicBool::new(false);

pub fn strings() -> &'static i18n::Strings {
    i18n::pick(LANG_JA.load(Ordering::Relaxed))
}

pub fn settings() -> settings::Settings {
    APP.get()
        .expect("APP is set at startup")
        .settings
        .lock()
        .expect("settings lock")
        .clone()
}

/// 設定を変えて、すぐ settings.toml に保存する。
pub fn update_settings(f: impl FnOnce(&mut settings::Settings)) {
    let app = APP.get().expect("APP is set at startup");
    let mut guard = app.settings.lock().expect("settings lock");
    f(&mut guard);
    match settings::save(&app.settings_path, &guard) {
        Ok(()) => eprintln!("settings: saved {}", app.settings_path.display()),
        Err(e) => eprintln!("settings: save failed: {e}"),
    }
}

pub fn set_language(language: &str) {
    update_settings(|s| s.language = language.to_string());
    LANG_JA.store(i18n::is_ja(language), Ordering::Relaxed);
    eprintln!("settings: language {language}");
    // メニューの表示名とピンのタスクバーの題は言語で変わる。
    apply_explorer_menu();
    pin::retitle();
    jumplist::update();
}

pub fn tray_hwnd() -> HWND {
    HWND(TRAY_HWND.load(Ordering::Relaxed) as *mut _)
}

/// 前面にする。ホットキーの後でも前面の権利が無いことがあるので、前面窓の入力に一時的に繋ぐ。
pub fn force_foreground(hwnd: HWND) {
    unsafe {
        if SetForegroundWindow(hwnd).as_bool() && GetForegroundWindow() == hwnd {
            return;
        }
        let fg = GetForegroundWindow();
        let fg_tid = GetWindowThreadProcessId(fg, None);
        let me = GetCurrentThreadId();
        let attached = fg_tid != 0 && fg_tid != me && AttachThreadInput(me, fg_tid, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(me, fg_tid, false);
        }
    }
}

static APP: OnceLock<App> = OnceLock::new();

fn main() {
    // COM: WIC・保存ダイアログ・Windows.Graphics.Capture。UI スレッドは STA。
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    #[cfg(debug_assertions)]
    if std::env::args().nth(1).as_deref() == Some("--probe") {
        #[cfg(feature = "insight")]
        insight::start();
        probe::run(std::env::args().skip(2).collect());
        return;
    }
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let open_path = open_arg(&args);
    let clip = clip_arg(&args);
    let task = jumplist::task_arg(&args);
    let mutex_name = HSTRING::from(format!("Local\\{PRODUCT_NAME}-single-instance"));
    // ハンドルは閉じない。プロセスが終わるまで mutex を持ち続ける。
    let _mutex = unsafe { CreateMutexW(None, true, &mutex_name) }.expect("CreateMutexW failed");
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        match (open_path, clip) {
            (Some(path), _) => {
                let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
                send_to_first(COPYDATA_OPEN, &wide, &format!("--open {}", path.display()));
            }
            (None, true) => send_to_first(COPYDATA_CLIP, &[], "ms-screenclip"),
            // 引数なし (exe をもう一度開いた) は設定を開かせる。
            (None, false) if args.is_empty() || task.is_some() => {
                let task = task.unwrap_or(jumplist::Task::Settings);
                let i = jumplist::Task::ALL
                    .iter()
                    .position(|&t| t == task)
                    .unwrap_or(0);
                send_to_first(COPYDATA_TASK, &[i as u16], &format!("{task:?}"));
            }
            (None, false) => eprintln!("already running; exiting"),
        }
        return;
    }
    migrate_from_odile();
    #[cfg(feature = "insight")]
    insight::start();

    let v2 = unsafe {
        AreDpiAwarenessContextsEqual(
            GetThreadDpiAwarenessContext(),
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        )
    }
    .as_bool();
    eprintln!("dpi awareness per-monitor v2: {v2}");

    let settings_path = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(PRODUCT_NAME)
        .join("settings.toml");
    let mut broken = false;
    let mut first_run = false;
    let mut settings = match settings::load(&settings_path) {
        Ok(settings::Loaded::Ok(s)) => s,
        Ok(settings::Loaded::Created(s)) => {
            eprintln!("settings: created {}", settings_path.display());
            first_run = true;
            s
        }
        Ok(settings::Loaded::Broken { settings, error }) => {
            eprintln!("settings: broken, backed up: {error}");
            broken = true;
            settings
        }
        Err(e) => {
            eprintln!("settings: {e}; using defaults");
            settings::Settings::default()
        }
    };
    if let Some(language) = take_installer_language() {
        settings.language = language.into();
        match settings::save(&settings_path, &settings) {
            Ok(()) => eprintln!("settings: language {language} from the installer"),
            Err(e) => eprintln!("settings: save failed: {e}"),
        }
    }
    LANG_JA.store(i18n::is_ja(&settings.language), Ordering::Relaxed);
    ui::set_show_in_capture(settings.show_in_capture);
    let strings = strings();
    APP.get_or_init(|| App {
        settings_path,
        settings: Mutex::new(settings.clone()),
    });

    let changed = |c: bool| if c { "written" } else { "unchanged" };
    match set_autostart(settings.autostart) {
        Ok(c) => eprintln!("autostart: {} ({})", settings.autostart, changed(c)),
        Err(e) => eprintln!("autostart: failed: {e}"),
    }
    apply_explorer_menu();
    match write_screenclip_handler() {
        Ok(c) => eprintln!("screenclip handler: registered ({})", changed(c)),
        Err(e) => eprintln!("screenclip handler: failed: {e}"),
    }

    let hwnd = create_window().expect("failed to create the tray window");
    TRAY_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
    add_tray_icon(hwnd).expect("Shell_NotifyIconW(NIM_ADD) failed");
    unsafe { SetTimer(Some(hwnd), TIMER_UPDATE, UPDATE_FIRST_MS, None) };

    let mut notices = Vec::new();
    if broken {
        notices.push(strings.settings_broken.to_string());
    }
    if settings.printscreen_takeover {
        match start_takeover() {
            Ok(hook) => *HOOK.lock().expect("hook lock") = Some(hook),
            Err(e) => {
                eprintln!("takeover: failed: {e}");
                notices.push(strings.takeover_failed.to_string());
            }
        }
    }
    if let Some(failed) = register_hotkeys(hwnd, &settings, strings, hooked()) {
        notices.push(failed);
    }
    if first_run {
        balloon(hwnd, &welcome(&settings, strings, notices), true);
    } else if !notices.is_empty() {
        notify(hwnd, &notices.join("\n"));
    }
    if let Some(path) = open_path {
        open_file(&path);
    } else if clip {
        eprintln!("screenclip: launched");
        start_clip(hwnd);
    } else if let Some(task) = task {
        run_task(hwnd, task);
    }
    jumplist::update();

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    gifpin::remove_temp_files();
    // フック用スレッドを止め、UnhookWindowsHookEx を済ませてから抜ける。
    stop_takeover();
}

/// 引数が `--open <path>` ならそのパス。エクスプローラのメニューは `"<exe>" --open "%1"` で起こす。
/// `--probe` (debug) はこれより先に分岐し、残りの引数を自分で読む。
fn open_arg(args: &[OsString]) -> Option<PathBuf> {
    match args {
        [flag, path, ..] if flag == "--open" => Some(PathBuf::from(path)),
        _ => None,
    }
}

/// Windows が `ms-screenclip:` を開くときの引数 (`ms-screenclip:///?source=PrintScreen`、
/// Win+Shift+S は `source=HotKey`)。どちらも静止画にするので source は読まない。
fn clip_arg(args: &[OsString]) -> bool {
    args.first()
        .and_then(|a| a.to_str())
        .and_then(|a| a.get(..14))
        .is_some_and(|p| p.eq_ignore_ascii_case("ms-screenclip:"))
}

/// ジャンプリストの頼みごと (2 本目から、または引数付きで起動した 1 本目)。送り手の SendMessage の中では
/// キャプチャ (COM) を呼べないので、タイマーで戻ってからする。
fn run_task(hwnd: HWND, task: jumplist::Task) {
    eprintln!("jumplist: {task:?}");
    let id = match task {
        jumplist::Task::Still => TIMER_TRAY_CLICK,
        jumplist::Task::Video => TIMER_JUMP_VIDEO,
        jumplist::Task::Settings => TIMER_JUMP_SETTINGS,
    };
    unsafe { SetTimer(Some(hwnd), id, TRAY_CLICK_DELAY_MS, None) };
}

/// 2 本目のプロセス: 1 本目のトレイの窓へ `WM_COPYDATA` (`kind` と `wide`) を渡す。
/// 窓が無ければ 1 本目が起動の途中なので、少し待って送り直す。
fn send_to_first(kind: usize, wide: &[u16], what: &str) {
    let class = HSTRING::from(format!("{PRODUCT_NAME}Tray"));
    let data = COPYDATASTRUCT {
        dwData: kind,
        cbData: (wide.len() * 2) as u32,
        lpData: wide.as_ptr() as *mut _,
    };
    for _ in 0..10 {
        if let Ok(hwnd) = unsafe { FindWindowW(&class, PCWSTR::null()) } {
            unsafe {
                // エクスプローラから貰った前面の権利を 1 本目へ渡す。無いとピンがエクスプローラの後ろに出る。
                let mut pid = 0;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                let _ = AllowSetForegroundWindow(pid);
                SendMessageTimeoutW(
                    hwnd,
                    WM_COPYDATA,
                    WPARAM(0),
                    LPARAM(&raw const data as isize),
                    SMTO_ABORTIFHUNG,
                    5000,
                    None,
                );
            }
            eprintln!("already running; sent {what}");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    eprintln!("already running; tray window not found, {what} dropped");
}

/// 大きさ `w`×`h` の画像の左上: 中心を `pt` に合わせ、`work` に収める。収まらなければ `work` の左上に揃える。
fn place(w: i32, h: i32, pt: POINT, work: RECT) -> POINT {
    let fit = |c: i32, len: i32, lo: i32, hi: i32| (c - len / 2).min(hi - len).max(lo);
    POINT {
        x: fit(pt.x, w, work.left, work.right),
        y: fit(pt.y, h, work.top, work.bottom),
    }
}

/// 画像か GIF のファイルを、マウスの位置にピンとして出す。読めなければ通知する。
/// 2 フレーム以上の GIF は GIF ピン、ほか (1 フレームの GIF を含む) は静止画ピン。
fn open_file(path: &Path) {
    eprintln!("open: {}", path.display());
    let pt = ui::cursor();
    let work = ui::work_area(ui::rect_at(pt, 1, 1));
    let result = (|| -> windows::core::Result<()> {
        let io = |e: std::io::Error| windows::core::Error::new(E_FAIL, e.to_string());
        if open::is_gif(path).map_err(io)? {
            // サムネイルの大きさは録画と同じ (record.rs)。倍率はマウスのあるモニタから取る。
            let s = ui::monitor_at(pt).dpi.max(96) as f32 / 96.0;
            let thumb = (ui::px(56.0, s).max(1) as u32, ui::px(32.0, s).max(1) as u32);
            if let Some(rec) = open::load_gif(path, thumb)? {
                // GIF ピンは拡大縮小できない。大きければ左上を作業領域に揃え、はみ出しは受け入れる。
                // 大きさは論理画面 (ヘッダの 6..10 バイト、LE の u16 × 2)。`load_gif` が展開した大きさと同じ。
                let mut head = [0u8; 10];
                std::io::Read::read_exact(&mut std::fs::File::open(path).map_err(io)?, &mut head)
                    .map_err(io)?;
                let w = i32::from(u16::from_le_bytes([head[6], head[7]]));
                let h = i32::from(u16::from_le_bytes([head[8], head[9]]));
                return gifpin::create(rec, ui::rect_at(place(w, h, pt, work), w, h), false);
            }
        }
        let image = open::load_image(path)?;
        // 作業領域より大きければ、収まる拡大率で出す。
        let (iw, ih) = (image.w as f32, image.h as f32);
        let zoom = ((work.right - work.left) as f32 / iw)
            .min((work.bottom - work.top) as f32 / ih)
            .min(1.0);
        let (w, h) = ((iw * zoom).round() as i32, (ih * zoom).round() as i32);
        pin::create_zoomed(image, place(w, h, pt, work), zoom)
    })();
    if let Err(e) = result {
        eprintln!("open: failed: {e}");
        let name = path.file_name().unwrap_or(path.as_os_str());
        notify(
            tray_hwnd(),
            &strings()
                .open_failed
                .replace("{name}", &name.to_string_lossy()),
        );
    }
}

static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
static TAKEOVER_HELD: AtomicBool = AtomicBool::new(false);
/// 横取りフックのスレッド (thread id, JoinHandle)。設定窓が入り切りする。
type HookThread = (u32, std::thread::JoinHandle<()>);
static HOOK: Mutex<Option<HookThread>> = Mutex::new(None);

fn hooked() -> bool {
    HOOK.lock().expect("hook lock").is_some()
}

fn stop_takeover() {
    let hook = HOOK.lock().expect("hook lock").take();
    if let Some((tid, thread)) = hook {
        let _ = unsafe { PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0)) };
        let _ = thread.join();
        eprintln!("takeover: hook removed");
    }
}

/// `printscreen_takeover` を実行中に入り切りする。ホットキーはフックの有無で登録する組が変わるので登録し直す。
/// フックを入れられなければ通知し、設定を OFF のまま Err を返す (設定窓はトグルを OFF に描く)。
pub fn set_takeover(on: bool) -> Result<(), String> {
    let strings = strings();
    if on && !hooked() {
        match start_takeover() {
            Ok(hook) => *HOOK.lock().expect("hook lock") = Some(hook),
            Err(e) => {
                eprintln!("takeover: failed: {e}");
                update_settings(|s| s.printscreen_takeover = false);
                let s = settings();
                let mut lines = vec![strings.takeover_failed.to_string()];
                lines.extend(prtsc_notice(&hotkey_texts(&s).map(String::as_str), false));
                notify(tray_hwnd(), &lines.join("\n"));
                return Err(e);
            }
        }
    } else if !on {
        stop_takeover();
    }
    update_settings(|s| s.printscreen_takeover = on);
    let results = reregister_hotkeys();
    let s = settings();
    let texts = hotkey_texts(&s).map(String::as_str);
    let failed: Vec<&str> = texts
        .iter()
        .zip(&results)
        .filter(|(_, r)| r.is_err())
        .map(|(t, _)| *t)
        .collect();
    let mut lines = Vec::new();
    if !failed.is_empty() {
        lines.push(format!("{} {}", strings.hotkey_failed, failed.join(", ")));
    }
    lines.extend(prtsc_notice(&texts, hooked()));
    if !lines.is_empty() {
        notify(tray_hwnd(), &lines.join("\n"));
    }
    Ok(())
}

const HOTKEY_IDS: std::ops::RangeInclusive<i32> = 1..=2;

/// 2 つのホットキーを外す (記録中は押したキーを設定窓へ届けるため)。
pub fn unregister_hotkeys() {
    for id in HOTKEY_IDS {
        let _ = unsafe { UnregisterHotKey(Some(tray_hwnd()), id) };
    }
}

/// 今の設定で 2 つを登録し直す。結果は 1 つずつ返す (添字は ACTIONS と同じ)。
pub fn reregister_hotkeys() -> [Result<(), String>; 2] {
    unregister_hotkeys();
    let s = settings();
    let texts = hotkey_texts(&s);
    let hooked = hooked();
    std::array::from_fn(|i| register_one(tray_hwnd(), i, texts[i], hooked).map(|_| ()))
}

/// ホットキー i (ACTIONS の添字) を `text` にする。登録に失敗したら前の値に戻して登録し直し、Err を返す。
/// Err は設定窓にそのまま出す文。
pub fn set_hotkey(i: usize, text: String) -> Result<(), String> {
    let strings = strings();
    let current = settings();
    let texts = hotkey_texts(&current).map(String::as_str);
    if let Some(j) = duplicate_of(&texts, i, &text) {
        eprintln!(
            "hotkey: {:?} rejected: {text} is the key of {:?}",
            ACTIONS[i], ACTIONS[j]
        );
        // 記録の開始で 2 つとも外している。拒んでも登録し直す。
        let _ = reregister_hotkeys();
        return Err(strings
            .hotkey_duplicate
            .replace("{a}", action_label(strings, j)));
    }
    let old = texts[i].to_string();
    let set = |t: String| {
        update_settings(|s| match i {
            0 => s.hotkey_still = t,
            _ => s.hotkey_video = t,
        })
    };
    set(text.clone());
    let results = reregister_hotkeys();
    if let Err(e) = &results[i] {
        eprintln!("hotkey: {:?} rejected, restoring {old}", ACTIONS[i]);
        set(old);
        let _ = reregister_hotkeys();
        return Err(if e == strings.hotkey_taken_by_takeover {
            e.clone()
        } else {
            strings.hotkey_in_use.replace("{key}", &text)
        });
    }
    if let Some(notice) = prtsc_notice(&[&text], hooked()) {
        notify(tray_hwnd(), &notice);
    }
    Ok(())
}

/// `texts` のうち i 以外で `text` と同じキーを持つ添字。未割当て (空) と読めない文字列は比べない。
/// 文字列は `hotkey::parse` → `format` で揃えてから比べる (手で書き換えた toml の修飾の順を吸収する)。
fn duplicate_of(texts: &[&str; 2], i: usize, text: &str) -> Option<usize> {
    let norm = |t: &str| {
        hotkey::parse(t)
            .ok()
            .flatten()
            .map(|h| hotkey::format(Some(h)))
    };
    let key = norm(text)?;
    (0..texts.len()).find(|&j| j != i && norm(texts[j]).as_ref() == Some(&key))
}

/// 設定窓のホットキーの行の名前 (添字は ACTIONS と同じ)。
fn action_label(strings: &i18n::Strings, i: usize) -> &'static str {
    [strings.hotkey_still, strings.hotkey_video][i]
}

pub fn hotkey_texts(settings: &settings::Settings) -> [&String; 2] {
    [&settings.hotkey_still, &settings.hotkey_video]
}

/// LL フックが飲む組み合わせ。修飾なし → 静止画、Shift だけ → 動画。
/// Alt (前面窓をクリップボードへ) / Win (ピクチャへ保存) / Ctrl を含むものは OS に渡す。
fn takeover_action(vk: u32, shift: bool, ctrl: bool, alt: bool, win: bool) -> Option<Action> {
    if vk != hotkey::VK_SNAPSHOT || ctrl || alt || win {
        return None;
    }
    Some(if shift { Action::Video } else { Action::Still })
}

/// LL フックを専用スレッドで入れる。成功したらそのスレッドの id を返す。
/// 専用スレッドにする理由: フックは入れたスレッドのメッセージループで呼ばれ、
/// `LowLevelHooksTimeout` を超えると黙って外される。メインスレッドの重い処理に巻き込まない。
fn start_takeover() -> Result<(u32, std::thread::JoinHandle<()>), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || unsafe {
        let mut msg = MSG::default();
        // スレッドのメッセージキューを作り、WM_QUIT の投函を受けられるようにする。
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
        let hook = match SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(takeover_hook),
            GetModuleHandleW(None).ok().map(Into::into),
            0,
        ) {
            Ok(hook) => hook,
            Err(e) => {
                let _ = tx.send(Err(e.message()));
                return;
            }
        };
        let _ = tx.send(Ok(GetCurrentThreadId()));
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        let _ = UnhookWindowsHookEx(hook);
    });
    let tid = rx.recv().map_err(|e| e.to_string())??;
    eprintln!("takeover: hook installed");
    Ok((tid, thread))
}

/// 判定と投函だけをしてすぐ返す。飲んだ組み合わせは KEYDOWN と KEYUP の両方を飲み、
/// dispatch は押されていない状態からの最初の KEYDOWN だけ (`MOD_NOREPEAT` 相当)。
/// `LLKHF_INJECTED` では絞らない (合成入力で検証するため。他ツールの合成 PrintScreen も取る)。
unsafe extern "system" fn takeover_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        if kb.vkCode == hotkey::VK_SNAPSHOT {
            if !down {
                if TAKEOVER_HELD.swap(false, Ordering::Relaxed) {
                    return LRESULT(1);
                }
            } else if TAKEOVER_HELD.load(Ordering::Relaxed) {
                return LRESULT(1);
            } else {
                let held = |vk: u16| unsafe { GetAsyncKeyState(vk as i32) } < 0;
                let action = takeover_action(
                    kb.vkCode,
                    held(VK_SHIFT.0),
                    held(VK_CONTROL.0),
                    held(VK_MENU.0),
                    held(VK_LWIN.0) || held(VK_RWIN.0),
                );
                if let Some(i) = action.and_then(|a| ACTIONS.iter().position(|&b| b == a)) {
                    TAKEOVER_HELD.store(true, Ordering::Relaxed);
                    let hwnd = HWND(TRAY_HWND.load(Ordering::Relaxed) as *mut _);
                    let _ = unsafe { PostMessageW(Some(hwnd), WM_TAKEOVER, WPARAM(i), LPARAM(0)) };
                    return LRESULT(1);
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 失敗があれば通知文を返す。
fn register_hotkeys(
    hwnd: HWND,
    settings: &settings::Settings,
    strings: &i18n::Strings,
    hooked: bool,
) -> Option<String> {
    let texts = hotkey_texts(settings).map(String::as_str);
    let failed: Vec<&str> = (0..texts.len())
        .filter(|&i| register_one(hwnd, i, texts[i], hooked).is_err())
        .map(|i| texts[i])
        .collect();
    let mut lines = Vec::new();
    if !failed.is_empty() {
        lines.push(format!("{} {}", strings.hotkey_failed, failed.join(", ")));
    }
    lines.extend(prtsc_notice(&texts, hooked));
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// `texts` のどれかが修飾なし PrintScreen で、横取りが OFF で、Snipping Tool が PrintScreen を取るときの案内
/// (`prtsc_taken` と `prtsc_hint`)。Win11 の Snipping が先に取り、WM_HOTKEY は来ない (RTX 4070 の机で実測。
/// 今は登録もしない)。修飾付きは届くので案内しない。ms-screenclip の既定が Manent なら届くので案内しない。
fn prtsc_notice(texts: &[&str], hooked: bool) -> Option<String> {
    let bare = texts.iter().any(|t| {
        matches!(hotkey::parse(t), Ok(Some(hk)) if hk.vk == hotkey::VK_SNAPSHOT && hk.mods == 0)
    });
    if !bare || hooked || !snipping_takes_prtsc() || screenclip_is_ours() {
        return None;
    }
    eprintln!("hotkey: PrintScreen is taken by Snipping Tool");
    let strings = strings();
    Some(format!("{}\n{}", strings.prtsc_taken, strings.prtsc_hint))
}

/// ホットキー i を登録する。`Ok(None)` は未割当てか、横取りフックが受ける組み合わせ。
/// フックが別の操作として受ける組み合わせは Err (`hotkey_taken_by_takeover`)。
fn register_one(
    hwnd: HWND,
    i: usize,
    text: &str,
    hooked: bool,
) -> Result<Option<hotkey::Hotkey>, String> {
    let action = ACTIONS[i];
    let result = hotkey::parse(text).and_then(|hk| {
        let Some(hk) = hk else { return Ok(None) };
        // フックが飲む組み合わせは WM_HOTKEY が来ないので登録しない (案内も出さない)。
        let has = |m: u32| hk.mods & m != 0;
        let mods = (
            has(hotkey::MOD_SHIFT),
            has(hotkey::MOD_CONTROL),
            has(hotkey::MOD_ALT),
            has(hotkey::MOD_WIN),
        );
        let taken = hooked
            .then(|| takeover_action(hk.vk, mods.0, mods.1, mods.2, mods.3))
            .flatten();
        // フックは素の PrintScreen を静止画、Shift+PrintScreen を動画に送る。ほかの操作には使えない。
        if taken.is_some_and(|a| a != action) {
            return Err(strings().hotkey_taken_by_takeover.to_string());
        }
        if taken.is_some() {
            eprintln!(
                "hotkey: {action:?} = {} is handled by the takeover hook",
                hotkey::format(Some(hk))
            );
            return Ok(None);
        }
        // Snipping が取る素の PrintScreen は、登録しても WM_HOTKEY が来ず、前面のアプリ
        // (FF14 のスクショ) からキーを奪うだけ (2026-11-08 RTX 4070 の机)。登録しない。案内は prtsc_notice が出す。
        if !hooked && hk.vk == hotkey::VK_SNAPSHOT && hk.mods == 0 && snipping_takes_prtsc() {
            eprintln!("hotkey: {action:?} = PrintScreen not registered (Snipping takes it)");
            return Ok(None);
        }
        unsafe {
            RegisterHotKey(
                Some(hwnd),
                i as i32 + 1,
                HOT_KEY_MODIFIERS(hk.mods) | MOD_NOREPEAT,
                hk.vk,
            )
        }
        .map(|()| Some(hk))
        .map_err(|e| e.message())
    });
    match &result {
        Ok(Some(hk)) => eprintln!(
            "hotkey registered: {action:?} = {}",
            hotkey::format(Some(*hk))
        ),
        Ok(None) => {}
        Err(e) => eprintln!("hotkey failed: {action:?} = {text}: {e}"),
    }
    result
}

/// `HKCU\Control Panel\Keyboard\PrintScreenKeyForSnippingEnabled`。
/// 値が無いときも ON とみなす (RTX 4070 の机の Win11 は値なしで Snipping が開いた)。
/// ponytail: 値なしで OFF が既定の Win10 / 古い Win11 では余計な通知が1回出る。
fn snipping_takes_prtsc() -> bool {
    let mut value = 1u32;
    let mut size = size_of::<u32>() as u32;
    let _ = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Control Panel\Keyboard"),
            w!("PrintScreenKeyForSnippingEnabled"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    value != 0
}

/// インストーラで選んだ言語 (`HKCU\Software\<製品名>\Language`、`ja` / `en`)。
/// 読んだら消す: 設定へ1回だけ移し、後で設定画面で変えた言語を次の更新で上書きしない。
/// インストーラは OS の UI 言語と違う言語を選んだときだけ書く (installer/manent.iss)。
fn take_installer_language() -> Option<&'static str> {
    let key = HSTRING::from(format!(r"Software\{PRODUCT_NAME}"));
    let mut buf = [0u16; 8];
    let mut size = size_of_val(&buf) as u32;
    let read = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &key,
            w!("Language"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    let _ = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &key, w!("Language")) };
    if read.is_err() {
        return None;
    }
    // size は終端の NUL を含むバイト数。
    let len = (size as usize / 2).saturating_sub(1);
    match String::from_utf16_lossy(&buf[..len]).as_str() {
        "ja" => Some("ja"),
        "en" => Some("en"),
        _ => None,
    }
}

fn create_window() -> windows::core::Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?.into();
        let class = HSTRING::from(format!("{PRODUCT_NAME}Tray"));
        ui::widgets::register_class(PCWSTR(class.as_ptr()), wndproc);
        // メッセージ専用ウインドウ (HWND_MESSAGE) ではなく、表示しないトップレベル窓にする。
        // メッセージ専用窓は前面にできず、トレイメニューが外側クリックで閉じない。
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            &class,
            &HSTRING::from(PRODUCT_NAME),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    }
}

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    }
}

fn copy_wide(dst: &mut [u16], text: &str) {
    let src: Vec<u16> = text.encode_utf16().take(dst.len() - 1).collect();
    dst[..src.len()].copy_from_slice(&src);
    dst[src.len()] = 0;
}

/// トレイに渡した今のマーク (`HICON`)。シェルは複製を持つので、渡した後は前のを壊してよい。
static TRAY_ICON: AtomicIsize = AtomicIsize::new(0);

/// `icon` を今のマークにし、前のを `DestroyIcon` する。null を渡すと今のを壊すだけ。
fn swap_tray_icon(icon: HICON) {
    let old = TRAY_ICON.swap(icon.0 as isize, Ordering::Relaxed);
    if old != 0 {
        let _ = unsafe { DestroyIcon(HICON(old as *mut _)) };
    }
}

fn add_tray_icon(hwnd: HWND) -> windows::core::Result<()> {
    let mut nid = tray_data(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = mark::tray_icon(mark::tray_size())?;
    TRAY_DPI.store(monitor_dpi(hwnd), Ordering::Relaxed);
    copy_wide(&mut nid.szTip, PRODUCT_NAME);
    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.ok();
    swap_tray_icon(nid.hIcon);
    added
}

/// タスクバーの明暗か DPI が変わった。トレイのマークを描き直す。
/// 大きさはいまのモニタの DPI から取る (`GetSystemMetrics` は system DPI の値で、その場の拡大率の変更を映さない)。
fn refresh_tray_icon(hwnd: HWND) {
    let dpi = monitor_dpi(hwnd);
    TRAY_DPI.store(dpi, Ordering::Relaxed);
    let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) }.max(16) as u32;
    let mut nid = tray_data(hwnd);
    nid.uFlags = NIF_ICON;
    match mark::tray_icon(size) {
        Ok(icon) => {
            nid.hIcon = icon;
            let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
            swap_tray_icon(icon);
            eprintln!(
                "tray: icon redrawn ({size} px, taskbar dark: {})",
                ui::taskbar_dark()
            );
        }
        Err(e) => eprintln!("tray: icon failed: {e}"),
    }
}

/// トレイのバルーン (失敗の知らせ)。続けて出すと前のものを置き換えるので、呼び手が1つにまとめる。
fn notify(hwnd: HWND, text: &str) {
    balloon(hwnd, text, false);
}

/// 初回の起動の案内。起動時の失敗の知らせ (`notices`) も同じバルーンに入れる (続けて出すと上書きされる)。
/// 知らせがあればキーと設定の行は出さない: 効かないかもしれないキーを案内せず、本文の上限 (255 文字) にも収める。
fn welcome(settings: &settings::Settings, strings: &i18n::Strings, notices: Vec<String>) -> String {
    let mut lines = vec![strings.welcome.to_string()];
    if notices.is_empty() {
        let keys = hotkey_texts(settings);
        for (key, line) in keys
            .into_iter()
            .zip([strings.welcome_still, strings.welcome_video])
        {
            if !key.is_empty() {
                lines.push(line.replace("{key}", key));
            }
        }
        lines.push(strings.welcome_tray.to_string());
    }
    lines.extend(notices);
    lines.join("\n")
}

/// `info` なら情報のアイコン (更新と初回の案内)、ほかは警告のアイコン。本文は 255 文字まで
/// (`i18n` の `startup_balloons_fit`)。
fn balloon(hwnd: HWND, text: &str, info: bool) {
    eprintln!("notice: {}", text.replace('\n', " / "));
    let mut nid = tray_data(hwnd);
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = if info { NIIF_INFO } else { NIIF_WARNING };
    copy_wide(&mut nid.szInfoTitle, PRODUCT_NAME);
    copy_wide(&mut nid.szInfo, text);
    if let Err(e) = unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) }.ok() {
        eprintln!("notice failed: {e}");
    }
}

/// 窓のいまのモニタの DPI。拡大率をその場で替えると、窓の `GetDpiForWindow` は動かすまで古いままの
/// ことがあるので、モニタから読む。この症状は `WS_EX_TOOLWINDOW` の窓で見つけた。ピンはいまはタスクバーに
/// 出る普通の窓だが、ほかの窓は tool window のままなので、どの窓でもモニタから読む。
pub fn monitor_dpi(hwnd: HWND) -> u32 {
    let (mut x, mut y) = (0, 0);
    let ok = unsafe {
        GetDpiForMonitor(
            MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
            MDT_EFFECTIVE_DPI,
            &mut x,
            &mut y,
        )
    }
    .is_ok();
    if ok && x > 0 { x } else { 96 }
}

/// トレイのマークを描いた DPI。同じ DPI の知らせでは描き直さない。
static TRAY_DPI: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY => {
            if let Some(action) = wparam.0.checked_sub(1).and_then(|i| ACTIONS.get(i)) {
                dispatch(*action);
            }
            LRESULT(0)
        }
        WM_TAKEOVER => {
            if let Some(action) = ACTIONS.get(wparam.0) {
                dispatch(*action);
            }
            LRESULT(0)
        }
        WM_COPYDATA => {
            let data = unsafe { &*(lparam.0 as *const COPYDATASTRUCT) };
            if data.dwData == COPYDATA_CLIP {
                // 送り手の SendMessage の中ではキャプチャ (COM) を呼べない
                // (RPC_E_CANTCALLOUT_ININPUTSYNCCALL、2026-11-08 RTX 4070 の机)。タイマーで戻ってから撮る。
                eprintln!("screenclip: received");
                start_clip(hwnd);
                return LRESULT(1);
            }
            if data.dwData == COPYDATA_TASK && data.cbData == 2 && !data.lpData.is_null() {
                let i = unsafe { *(data.lpData as *const u16) } as usize;
                if let Some(&task) = jumplist::Task::ALL.get(i) {
                    run_task(hwnd, task);
                }
                return LRESULT(1);
            }
            if data.dwData != COPYDATA_OPEN || data.lpData.is_null() {
                return LRESULT(0);
            }
            // 送り手のバッファは戻るまでしか生きない。先に写す。
            let wide = unsafe {
                std::slice::from_raw_parts(data.lpData as *const u16, data.cbData as usize / 2)
            };
            let path = PathBuf::from(OsString::from_wide(wide));
            open_file(&path);
            LRESULT(1)
        }
        WM_UIA => {
            overlay::on_uia(uia::take(lparam));
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            // "ImmersiveColorSet": アプリとタスクバーの明暗が変わった。
            let topic = if lparam.0 == 0 {
                String::new()
            } else {
                unsafe { PCWSTR(lparam.0 as *const u16).to_string() }.unwrap_or_default()
            };
            if topic == "ImmersiveColorSet" {
                refresh_tray_icon(hwnd);
                pin::theme_changed();
                gifpin::theme_changed();
            }
            LRESULT(0)
        }
        // 拡大率をその場で替えると、ツール窓 (トレイ・ピン・GIF ピン・HUD) には WM_DPICHANGED が来ない。
        // 全部の窓に来るのは WM_SETTINGCHANGE (0x9F・0x2F) と WM_DISPLAYCHANGE (2026-11-04 RTX 4070 の机、
        // dpi.ps1 でも設定アプリでも同じ)。ここで 1 回、モニタの DPI を読み直す。
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            if TRAY_DPI.load(Ordering::Relaxed) != monitor_dpi(hwnd) {
                refresh_tray_icon(hwnd);
            }
            pin::dpi_changed();
            gifpin::dpi_changed();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_CLIP => {
            let waited = CLIP_T0
                .lock()
                .expect("clip lock")
                .map_or(CLIP_WAIT_MAX, |t| t.elapsed());
            let down = unsafe { GetAsyncKeyState(hotkey::VK_SNAPSHOT as i32) } < 0;
            if down && waited < CLIP_WAIT_MAX {
                return LRESULT(0);
            }
            let _ = unsafe { KillTimer(Some(hwnd), TIMER_CLIP) };
            eprintln!(
                "screenclip: start after {} ms ({})",
                waited.as_millis(),
                if down { "still down" } else { "released" }
            );
            dispatch(Action::Still);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_UPDATE => {
            unsafe { SetTimer(Some(hwnd), TIMER_UPDATE, UPDATE_EVERY_MS, None) };
            check_update();
            LRESULT(0)
        }
        WM_UPDATE => {
            on_update(hwnd, *update::take(lparam));
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_JUMP_VIDEO => {
            let _ = unsafe { KillTimer(Some(hwnd), TIMER_JUMP_VIDEO) };
            dispatch(Action::Video);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_JUMP_SETTINGS => {
            let _ = unsafe { KillTimer(Some(hwnd), TIMER_JUMP_SETTINGS) };
            setwin::show();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_TRAY_CLICK => {
            let _ = unsafe { KillTimer(Some(hwnd), TIMER_TRAY_CLICK) };
            dispatch(Action::Still);
            LRESULT(0)
        }
        WM_TRAY => {
            match lparam.0 as u32 & 0xFFFF {
                // 凍結はフライアウト (溢れたアイコンの一覧) が閉じてから。クリックの中で凍結すると写る。
                // ダブルクリックは同じ id のタイマーを掛け直すので 1 回。
                WM_LBUTTONUP => {
                    eprintln!("tray: left click");
                    unsafe { SetTimer(Some(hwnd), TIMER_TRAY_CLICK, TRAY_CLICK_DELAY_MS, None) };
                }
                WM_RBUTTONUP => traymenu::show(),
                // どのバルーンも、クリックで設定窓を開く。失敗の知らせの直し方は設定窓にある (`prtsc_hint`)。
                NIN_BALLOONUSERCLICK => setwin::show(),
                _ => {}
            }
            LRESULT(0)
        }
        // インストーラ (Restart Manager) やログオフの終了要求。窓を壊して WM_DESTROY から抜ける。
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &tray_data(hwnd));
                swap_tray_icon(HICON::default());
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 設定で確認がオンなら、別スレッドで最新の版を調べる。オフなら外へは何も送らない。
/// ストア版では何もしない (ストアの規約がストア外からの自己更新を禁じる。更新はストアに任せる)。
pub fn check_update() {
    if !is_store() && settings().update_check {
        update::check_async();
    }
}

fn on_update(hwnd: HWND, msg: update::Msg) {
    // 取得中に 24 時間の確認が届いても状態を戻さない (Available に戻ると二重に取得できる)。
    if matches!(msg, update::Msg::Checked(_))
        && matches!(update::status(), update::Status::Downloading(_))
    {
        eprintln!("update: check ignored while downloading");
        return;
    }
    // 取得と起動の失敗: 取得の前の Available に戻し、もう一度押せるようにする。
    let retry = || {
        update::set_status(match update::status() {
            update::Status::Downloading(r) => update::Status::Available(r),
            _ => update::Status::Failed,
        })
    };
    match msg {
        update::Msg::Checked(Ok(release)) => {
            let current = env!("CARGO_PKG_VERSION");
            let newer = update::is_newer(&release.version, current) == Some(true);
            eprintln!(
                "update: latest {} (current {current}, newer: {newer})",
                release.version
            );
            if !newer {
                update::set_status(update::Status::Latest);
            } else {
                let mut shown = UPDATE_NOTIFIED.lock().expect("update lock");
                if shown.as_deref() != Some(release.version.as_str()) {
                    *shown = Some(release.version.clone());
                    balloon(
                        hwnd,
                        &strings().update_available.replace("{v}", &release.version),
                        true,
                    );
                }
                update::set_status(update::Status::Available(release));
            }
        }
        update::Msg::Checked(Err(e)) => {
            eprintln!("update: check failed: {e}");
            update::set_status(update::Status::Failed);
        }
        update::Msg::Downloaded(Ok(path)) => {
            let r = unsafe {
                ShellExecuteW(
                    None,
                    w!("open"),
                    &HSTRING::from(path.as_os_str()),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    SW_SHOWNORMAL,
                )
            };
            eprintln!("update: launched installer ({})", r.0 as isize);
            if r.0 as isize > 32 {
                // インストーラは CloseApplications=force。待たずに抜ける。
                let _ = unsafe { DestroyWindow(hwnd) };
                return;
            }
            retry();
        }
        update::Msg::Downloaded(Err(e)) => {
            eprintln!("update: download failed: {e}");
            retry();
        }
    }
    setwin::update_changed();
}

/// 旧製品名 Odile の名残を新しい名前へ移す (2026-11-06 改名)。
/// %APPDATA% と %LOCALAPPDATA% のフォルダは改名する。新しい名前のフォルダが既にあれば触らない。
/// 自動起動の値と右クリックのメニューは消すだけ。新しい名前で書くのは後の `set_autostart` と `apply_explorer_menu`。
// ponytail: 改名直後の移行用。旧版の利用者が一度起動し終えたら消してよい。
/// ストア版では何もしない: 旧名 Odile の版は GitHub 版だけで、パッケージの中の HKCU の削除は外に届かない。
fn migrate_from_odile() {
    if is_store() {
        return;
    }
    const OLD: &str = "Odile";
    for var in ["APPDATA", "LOCALAPPDATA"] {
        let Some(base) = std::env::var_os(var).map(PathBuf::from) else {
            continue;
        };
        let (old, new) = (base.join(OLD), base.join(PRODUCT_NAME));
        if old.is_dir() && !new.exists() {
            match std::fs::rename(&old, &new) {
                Ok(()) => eprintln!("migrate: {} -> {}", old.display(), new.display()),
                Err(e) => eprintln!("migrate: {}: {e}", old.display()),
            }
        }
    }
    unsafe {
        let menu = format!(r"Software\Classes\SystemFileAssociations\image\shell\{OLD}");
        // 鍵・値が無ければどちらも ERROR_FILE_NOT_FOUND で何も書かない。
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(menu));
        let _ = RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
            &HSTRING::from(OLD),
        );
    }
}

/// package identity があるか (ストア版の MSIX から起きたか)。1 回だけ調べて覚える。
/// identity があれば長さを問う呼び出しが ERROR_INSUFFICIENT_BUFFER、無ければ APPMODEL_ERROR_NO_PACKAGE (15700)。
pub fn is_store() -> bool {
    static STORE: OnceLock<bool> = OnceLock::new();
    *STORE.get_or_init(|| {
        let mut len = 0u32;
        let store =
            unsafe { GetCurrentPackageFullName(&mut len, None) } == ERROR_INSUFFICIENT_BUFFER;
        eprintln!("package identity: {store}");
        store
    })
}

/// マニフェストの `desktop:StartupTask` の TaskId。
const STARTUP_TASK: &str = "ManentStartup";

/// ストア版の `StartupTask` で `f` を走らせる。待つ間に UI の STA を挟まないよう、別のスレッドで呼んで待つ。
fn with_startup_task<T: Send + 'static>(
    f: impl FnOnce(&StartupTask) -> windows::core::Result<T> + Send + 'static,
) -> Result<T, String> {
    std::thread::spawn(move || {
        let task = StartupTask::GetAsync(&HSTRING::from(STARTUP_TASK))?.join()?;
        f(&task)
    })
    .join()
    .map_err(|_| "startup task thread panicked".to_string())?
    .map_err(|e| e.message())
}

/// ストア版の設定窓の自動起動のトグルに出す値。`DisabledByUser` (「設定 > スタートアップ」で切られた) は OFF。
pub fn startup_task_enabled() -> bool {
    with_startup_task(|t| t.State()).is_ok_and(|s| {
        matches!(
            s,
            StartupTaskState::Enabled | StartupTaskState::EnabledByPolicy
        )
    })
}

/// 自動起動を `enabled` に合わせる。戻り値は「変えたか」。
/// GitHub 版は `HKCU\...\Run` の値 (名前は製品名)。ストア版は `StartupTask`
/// (パッケージの中の HKCU の書き込みは仮想化され、外の Run に届かない)。
pub fn set_autostart(enabled: bool) -> Result<bool, String> {
    if is_store() {
        // ユーザーやポリシーが決めた状態はアプリから変えない。設定の `autostart` も書き換えない
        // (共有の値を false にすると、GitHub 版が次の起動で Run の値を消すため)。
        return with_startup_task(move |t| {
            let state = t.State()?;
            eprintln!("autostart: startup task state {}", state.0);
            Ok(match state {
                StartupTaskState::Enabled if !enabled => {
                    t.Disable()?;
                    true
                }
                StartupTaskState::Disabled if enabled => {
                    t.RequestEnableAsync()?.join()?;
                    true
                }
                _ => false,
            })
        });
    }
    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut value = OsString::from("\"");
        value.push(exe.as_os_str());
        value.push("\"");
        return set_values(&[(RUN.into(), PRODUCT_NAME, value)]);
    }
    let name = HSTRING::from(PRODUCT_NAME);
    match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(RUN), &name) } {
        ERROR_FILE_NOT_FOUND => Ok(false),
        other => other.ok().map(|()| true).map_err(|e| e.message()),
    }
}

/// 設定の `explorer_menu` と今の言語で、エクスプローラの右クリックのメニューを書き直すか消す。
/// ストア版は HKCU に書かない。メニューはマニフェストの DLL が出し、同じ設定ファイルを読んで消す。
pub fn apply_explorer_menu() {
    let on = settings().explorer_menu;
    if is_store() {
        eprintln!("explorer menu: {on} (store build, manifest)");
        return;
    }
    match set_explorer_menu(on) {
        Ok(true) => eprintln!("explorer menu: {on} (written, SHChangeNotify sent)"),
        Ok(false) => eprintln!("explorer menu: {on} (unchanged, no SHChangeNotify)"),
        Err(e) => eprintln!("explorer menu: failed: {e}"),
    }
}

/// `HKCU\Software\Classes\SystemFileAssociations\image\shell\<製品名>` を `enabled` に合わせる。
/// `image` は PerceivedType なので PNG・JPEG・GIF などにまとめて出る (Win11 では「その他のオプションを確認」の先)。
/// 書き換えたときだけ `SHCNE_ASSOCCHANGED` を送り、Explorer に関連付けを読み直させる。
/// 戻り値は「書き換えたか」。値が同じなら書かず送らない (起動のたびに Explorer のキャッシュを捨てさせない)。
fn set_explorer_menu(enabled: bool) -> Result<bool, String> {
    let changed = write_explorer_menu(enabled)?;
    if changed {
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
    }
    Ok(changed)
}

/// 戻り値は「変えたか」。消すときは鍵が無ければ変えていない。
fn write_explorer_menu(enabled: bool) -> Result<bool, String> {
    let key = format!(r"Software\Classes\SystemFileAssociations\image\shell\{PRODUCT_NAME}");
    unsafe {
        if !enabled {
            return match RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(key)) {
                ERROR_FILE_NOT_FOUND => Ok(false),
                other => other.ok().map(|()| true).map_err(|e| e.message()),
            };
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut command = OsString::from("\"");
        command.push(exe.as_os_str());
        command.push("\" --open \"%1\"");
        let label = OsString::from(strings().explorer_verb.replace("{app}", PRODUCT_NAME));
        set_values(&[
            (key.clone(), "MUIVerb", label),
            (key.clone(), "Icon", exe.into_os_string()),
            (format!(r"{key}\command"), "", command),
        ])
    }
}

/// `ms-screenclip:` の受け口を HKCU に書く。Windows の「既定のアプリ > リンクの種類」の
/// MS-SCREENCLIP に Manent が候補として出るだけで、既定はユーザーが選ぶまで Snipping Tool のまま。
/// 既定にすると PrintScreen と Win+Shift+S で `"<exe>" "ms-screenclip:///?source=…"` が起動する
/// (2026-11-08 RTX 4070 の机で実測)。exe の場所が変わっても追うよう、起動のたびに比べて違えば書き直す。
/// 戻り値は「書いたか」。ストア版は書かない (受け口はマニフェストの `uap3:Protocol`)。
fn write_screenclip_handler() -> Result<bool, String> {
    if is_store() {
        return Ok(false);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let caps = format!(r"Software\{PRODUCT_NAME}\Capabilities");
    let prog = format!(r"Software\Classes\{PRODUCT_NAME}.ScreenClip");
    let mut command = OsString::from("\"");
    command.push(exe.as_os_str());
    command.push("\" \"%1\"");
    let name = OsString::from(PRODUCT_NAME);
    set_values(&[
        (caps.clone(), "ApplicationName", name.clone()),
        (
            format!(r"{caps}\URLAssociations"),
            "ms-screenclip",
            OsString::from(format!("{PRODUCT_NAME}.ScreenClip")),
        ),
        (
            r"Software\RegisteredApplications".into(),
            PRODUCT_NAME,
            caps.into(),
        ),
        (prog.clone(), "", OsString::from("URL:ms-screenclip")),
        (format!(r"{prog}\Application"), "ApplicationName", name),
        (format!(r"{prog}\DefaultIcon"), "", exe.into_os_string()),
        (format!(r"{prog}\shell\open\command"), "", command),
    ])
}

/// `ms-screenclip:` の既定が Manent か。ユーザーが既定のアプリで選んでも `UserChoice` の ProgId は
/// AppX のままだったが、`AssocQueryString` は選んだ ProgId を返した (2026-11-08 RTX 4070 の机)。
pub fn screenclip_is_ours() -> bool {
    let mut buf = [0u16; 128];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        AssocQueryStringW(
            ASSOCF_IS_PROTOCOL,
            ASSOCSTR_PROGID,
            w!("ms-screenclip"),
            PCWSTR::null(),
            Some(windows::core::PWSTR(buf.as_mut_ptr())),
            &mut len,
        )
    }
    .is_ok();
    let progid = String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1).min(buf.len())]);
    ok && progid == format!("{PRODUCT_NAME}.ScreenClip")
}

/// (HKCU 下のキー, 値の名前, 文字列) を REG_SZ で書く。今の値と同じものは書かない。
/// 戻り値は「1 つでも書いたか」。
fn set_values(values: &[(String, &str, OsString)]) -> Result<bool, String> {
    let mut wrote = false;
    unsafe {
        for (sub, name, value) in values {
            let (sub, name) = (HSTRING::from(sub.as_str()), HSTRING::from(*name));
            let wide: Vec<u16> = value.encode_wide().chain([0]).collect();
            // 1 文字多く読める大きさで読む。長い値は ERROR_MORE_DATA、短い値は size で違いが分かる。
            let mut buf = vec![0u16; wide.len() + 1];
            let mut size = (buf.len() * 2) as u32;
            let same = RegGetValueW(
                HKEY_CURRENT_USER,
                &sub,
                &name,
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
            .is_ok()
                && size as usize == wide.len() * 2
                && buf[..wide.len()] == wide[..];
            if same {
                continue;
            }
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &sub,
                &name,
                REG_SZ.0,
                Some(wide.as_ptr().cast()),
                (wide.len() * 2) as u32,
            )
            .ok()
            .map_err(|e| e.message())?;
            wrote = true;
        }
    }
    Ok(wrote)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本物の HKCU に書く。同じ値は書かない (起動のたびの書き込みと SHChangeNotify を止める根拠)。
    #[test]
    fn set_values_writes_only_on_change() {
        let root = format!(
            r"Software\{PRODUCT_NAME}-test-set-values-{}",
            std::process::id()
        );
        let sub = format!(r"{root}\k");
        let v = |s: &str| [(sub.clone(), "", OsString::from(s))];
        // (値, 書いたか): 初回・同じ・長く・短く・同じ・空。
        let rows = [
            ("abc", true),
            ("abc", false),
            ("abcd", true),
            ("ab", true),
            ("ab", false),
            ("", true),
            ("", false),
        ];
        let got: Vec<_> = rows.iter().map(|(s, _)| set_values(&v(s))).collect();
        let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(root)) };
        for ((s, want), got) in rows.iter().zip(got) {
            assert_eq!(got, Ok(*want), "{s:?}");
        }
    }

    #[test]
    fn open_arg_table() {
        let rows: [(&[&str], Option<&str>); 5] = [
            (
                &["--open", r"C:\My Pictures\a b.png"],
                Some(r"C:\My Pictures\a b.png"),
            ),
            (&["--open"], None),
            (&[], None),
            (&["--probe", "--open", "x.png"], None),
            (&["x.png", "--open"], None),
        ];
        for (args, want) in rows {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            assert_eq!(open_arg(&args), want.map(PathBuf::from), "{args:?}");
        }
    }

    #[test]
    fn clip_arg_table() {
        let rows: [(&[&str], bool); 6] = [
            (&["ms-screenclip:///?source=PrintScreen"], true),
            (&["MS-ScreenClip:///?source=HotKey"], true),
            (&["ms-screenclip:"], true),
            (&["--open", "ms-screenclip:x"], false),
            (&["ms-screen"], false),
            // 14 バイト目が文字の途中に来る引数でも落ちない。
            (&["あいうえおか.png"], false),
        ];
        for (args, want) in rows {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            assert_eq!(clip_arg(&args), want, "{args:?}");
        }
    }

    #[test]
    fn place_table() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1000,
            bottom: 800,
        };
        let pt = |x, y| POINT { x, y };
        // (w, h, マウス, 左上): 中心に置く・下端と左端・右端と上端に寄せる・大きすぎれば左上。
        let rows = [
            (100, 50, pt(500, 400), pt(450, 375)),
            (100, 50, pt(10, 790), pt(0, 750)),
            (100, 50, pt(990, 5), pt(900, 0)),
            (1500, 900, pt(500, 400), pt(0, 0)),
        ];
        for (w, h, at, want) in rows {
            let got = place(w, h, at, work);
            assert_eq!((got.x, got.y), (want.x, want.y), "{w}x{h} at {at:?}");
        }
    }

    #[test]
    fn duplicate_of_table() {
        let texts = ["Ctrl+Alt+Shift+S", ""];
        // (i, text, want)
        let rows = [
            (1, "Ctrl+Alt+Shift+S", Some(0)),
            (1, "Shift+Ctrl+Alt+S", Some(0)),
            (0, "Ctrl+Alt+Shift+S", None),
            (0, "", None),
            (1, "F13", None),
        ];
        for (i, text, want) in rows {
            assert_eq!(duplicate_of(&texts, i, text), want, "{i} {text:?}");
        }
    }

    #[test]
    fn takeover_action_table() {
        const P: u32 = hotkey::VK_SNAPSHOT;
        // (vk, shift, ctrl, alt, win)
        let rows = [
            ((P, false, false, false, false), Some(Action::Still)),
            ((P, true, false, false, false), Some(Action::Video)),
            ((P, false, false, true, false), None),
            ((P, false, false, false, true), None),
            ((P, false, true, false, false), None),
            ((P, true, true, false, false), None),
            ((P, true, false, true, false), None),
            ((0x53, false, false, false, false), None),
            ((0x53, true, false, false, false), None),
        ];
        for ((vk, shift, ctrl, alt, win), want) in rows {
            assert_eq!(
                takeover_action(vk, shift, ctrl, alt, win),
                want,
                "{vk:#x} s{shift} c{ctrl} a{alt} w{win}"
            );
        }
    }
}
