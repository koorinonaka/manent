//! 手順 1 の実測用 (debug ビルドだけ)。`manent --probe <freeze|uia|acrylic> …`。
//! 結果は stderr に出す。手順と値は KNOWLEDGE「ガラス HUD と凍結の実測」。

use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, w};

use crate::ui::{self, widgets};

pub fn run(args: Vec<String>) {
    match args.first().map(String::as_str) {
        Some("freeze") => freeze(args.get(1).and_then(|n| n.parse().ok()).unwrap_or(5)),
        Some("uia") => uia(),
        Some("acrylic") => acrylic(&args[1..]),
        Some("mark") => mark(args.get(1).map(String::as_str).unwrap_or(".")),
        _ => eprintln!(
            "usage: --probe freeze [n] | uia | acrylic <clear|opaque|gdi…> <x> <y> [selffg|activate|attr|accent4|tint|small|w=N|h=N|secs=N]"
        ),
    }
}

/// トレイのマークを 16・20・24 と両方のインクで PNG に書く。
fn mark(dir: &str) {
    for size in [16, 20, 24] {
        for (dark, name) in [(true, "dark-taskbar"), (false, "light-taskbar")] {
            let bgra = crate::mark::pixels(size, dark).expect("mark");
            let path = std::path::Path::new(dir).join(format!("tray-mark-{name}-{size}.png"));
            let image = crate::capture::Image {
                w: size,
                h: size,
                bgra,
            };
            crate::export::write_png(&path, &image).expect("png");
            println!("{}", path.display());
        }
    }
}

fn freeze(n: usize) {
    // アプリは起動時にトレイのマークを描くので、デバイスは凍結の前にできている。
    let t = Instant::now();
    let _ = ui::gfx();
    println!("device init: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    for i in 0..n {
        let t = Instant::now();
        match crate::capture::freeze(false) {
            Ok(shots) => println!(
                "freeze {i}: {:.1} ms, {} monitor(s) {:?}",
                t.elapsed().as_secs_f64() * 1000.0,
                shots.len(),
                shots.iter().map(|s| (s.w, s.h)).collect::<Vec<_>>()
            ),
            Err(e) => println!("freeze {i}: error {e}"),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // `--insight` の集計 (5 秒ごと) を書かせてから終わる。
    #[cfg(feature = "insight")]
    std::thread::sleep(Duration::from_millis(5500));
}

fn uia() {
    let notify = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            &HSTRING::new(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    }
    .expect("message window");
    let tx = crate::uia::start(notify, crate::WM_UIA);
    let windows = crate::overlay::top_windows();
    for (seq, win) in windows.iter().take(10).enumerate() {
        let r = win.rect;
        let pt = POINT {
            x: (r.left + r.right) / 2,
            y: r.top + (r.bottom - r.top) / 3,
        };
        let sent = Instant::now();
        let _ = tx.send(crate::uia::Request { hwnd: win.hwnd, pt });
        let mut msg = MSG::default();
        loop {
            if !unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
                return;
            }
            if msg.message == crate::WM_UIA {
                let a = crate::uia::take(msg.lParam);
                let deepest = a
                    .chain
                    .first()
                    .map(|e| format!("{} '{}'", e.kind, e.name))
                    .unwrap_or_default();
                println!(
                    "uia {seq}: query {:.1} ms, round trip {:.1} ms, depth {}, title chars {}, deepest {}",
                    a.ms,
                    sent.elapsed().as_secs_f64() * 1000.0,
                    a.chain.len(),
                    win.title.chars().count(),
                    deepest
                );
                break;
            }
            unsafe {
                DispatchMessageW(&msg);
            }
        }
    }
}

unsafe extern "system" fn plain_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // 枠 (WS_THICKFRAME) のある WS_POPUP は、非クライアント域を 0 にして全面をクライアントにする。
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
    if msg == WM_NCCALCSIZE
        && wparam.0 != 0
        && style & WS_POPUP.0 != 0
        && style & WS_THICKFRAME.0 != 0
    {
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// GDI の窓 (リダイレクトあり、DirectComposition なし) に backdrop を付ける。地は黒 = 拡張枠の中では透明。
fn gdi_window(x: i32, y: i32, noactivate: bool, framed: bool, thick: bool) -> HWND {
    use windows::Win32::Graphics::Dwm::*;
    use windows::Win32::Graphics::Gdi::{BLACK_BRUSH, GetStockObject, HBRUSH};
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(plain_proc),
            lpszClassName: w!("OdileProbeGdi"),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let mut ex = WS_EX_TOOLWINDOW | WS_EX_TOPMOST;
        if noactivate {
            ex |= WS_EX_NOACTIVATE;
        }
        // framed: 普通の枠付きの窓 (backdrop が出るはずの形) を陽性の対照にする。
        let (style, x, y, w, h) = if framed {
            (WS_OVERLAPPEDWINDOW, x - 10, y - 30, 320, 120)
        } else if thick {
            (WS_POPUP | WS_THICKFRAME, x, y, 289, 40)
        } else {
            (WS_POPUP, x, y, 289, 40)
        };
        let hwnd = CreateWindowExW(
            ex,
            w!("OdileProbeGdi"),
            &HSTRING::new(),
            style,
            x,
            y,
            w,
            h,
            None,
            None,
            None,
            None,
        )
        .expect("gdi window");
        let _ = windows::Win32::Graphics::Dwm::DwmExtendFrameIntoClientArea(
            hwnd,
            &windows::Win32::UI::Controls::MARGINS {
                cxLeftWidth: -1,
                cxRightWidth: -1,
                cyTopHeight: -1,
                cyBottomHeight: -1,
            },
        );
        let ok =
            widgets::set_attr(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &DWMSBT_TRANSIENTWINDOW).is_ok();
        let _ = widgets::set_attr(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_ROUND);
        let _ = widgets::set_attr(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &windows::core::BOOL::from(ui::apps_dark()),
        );
        println!(
            "acrylic: gdi window, framed {framed}, thick {thick}, noactivate {noactivate}, backdrop attribute accepted: {ok}"
        );
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        hwnd
    }
}

/// acrylic の窓を (x, y) に出し、20 秒 (`secs=N`) そのままにする。撮影と輝度の統計はスクリプト側。
/// mode: clear (DComp、透明) / opaque (DComp、surface-opaque) / gdi (GDI、NOACTIVATE) / gdiact (GDI、NOACTIVATE なし)。
/// 旗 (t261104-ca82 の手順 1 の arm): `attr` は作った後に `DWMSBT_TRANSIENTWINDOW` を付ける、`accent4` は accent を
/// ACRYLICBLURBEHIND に替える、`tint` は地を `surface-acrylic` で塗る、`small` は角を `DWMWCP_ROUNDSMALL`、
/// `w=N` / `h=N` は大きさ (既定 289 × 40)。accent を外すのは環境変数 `ODILE_NO_BLUR=1`。
fn acrylic(args: &[String]) {
    use windows::Win32::Graphics::Dwm::*;
    let mode = args.first().map(String::as_str).unwrap_or("clear");
    let x: i32 = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(200);
    let y: i32 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(200);
    let flags = args.get(3..).unwrap_or_default();
    let has = |f: &str| flags.iter().any(|v| v == f);
    let num = |k: &str, d: i32| {
        flags
            .iter()
            .find_map(|v| v.strip_prefix(k).and_then(|n| n.parse().ok()))
            .unwrap_or(d)
    };
    let (w, h) = (num("w=", 289), num("h=", 40));
    let self_fg = has("selffg");
    let activate = has("activate");
    widgets::register_class(w!("OdileProbe"), plain_proc);
    let mut keep = None;
    let target = if mode.starts_with("gdi") || mode == "framed" || mode.starts_with("thick") {
        gdi_window(
            x,
            y,
            mode == "gdi" || mode == "thick",
            mode == "framed",
            mode.starts_with("thick"),
        )
    } else {
        let mut win =
            widgets::Acrylic::new(w!("OdileProbe"), has("small"), widgets::Capture::Never)
                .expect("acrylic");
        if has("accent4") {
            win.blur = widgets::set_accent(
                win.hwnd,
                widgets::ACCENT_ENABLE_ACRYLICBLURBEHIND,
                0x0100_0000,
            );
        }
        if has("attr") {
            let ok =
                widgets::set_attr(win.hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &DWMSBT_TRANSIENTWINDOW)
                    .is_ok();
            println!("acrylic: backdrop attribute accepted: {ok}");
        }
        println!("acrylic: blur behind: {}", win.blur);
        win.show_at(RECT {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        });
        let pal = ui::palette();
        let ground = if mode == "opaque" {
            Some(pal.surface_opaque)
        } else if has("tint") {
            Some(pal.surface_acrylic)
        } else {
            None
        };
        win.surface.draw(|ctx| {
            if let Some(c) = ground {
                ui::fill_rect(ctx, ui::rectf(0.0, 0.0, w as f32, h as f32), c)
            }
        });
        let h = win.hwnd;
        keep = Some(win);
        h
    };
    if self_fg {
        // 自アプリの普通の窓を前面にする (条件 a: 自分が前面、acrylic の窓は前面を取らない)。
        let main = unsafe {
            CreateWindowExW(
                Default::default(),
                w!("OdileProbe"),
                &HSTRING::from("probe-fg"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                x,
                y + 300,
                300,
                120,
                None,
                None,
                None,
                None,
            )
        }
        .expect("fg window");
        crate::force_foreground(main);
    }
    if activate {
        // 陽性の対照: acrylic の窓そのものをアクティブにする。
        crate::force_foreground(target);
    }
    let fg = unsafe { GetForegroundWindow() };
    println!(
        "acrylic: foreground is the acrylic window: {}",
        fg == target
    );
    println!("acrylic: shown at ({x}, {y}); foreground is ours: {}", {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(fg, Some(&mut pid)) };
        pid == std::process::id()
    });
    let until = Instant::now() + Duration::from_secs(num("secs=", 20) as u64);
    let mut msg = MSG::default();
    while Instant::now() < until {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(keep);
}
