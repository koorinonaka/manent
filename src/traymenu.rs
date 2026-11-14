//! トレイのアイコンの右クリックで開くメニュー (DS `TrayMenu`)。前面を取る acrylic の窓。
//! 外のクリック (`WA_INACTIVE`) と Esc で何もせずに閉じる。キャプチャには写らない。
//! ピンの右クリックのメニューも同じ窓と行で出す (`popup`)。

use std::cell::RefCell;

use windows::Win32::Foundation::*;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::ui::widgets::{self, Acrylic, BtnState, Capture, Tooltip};
use crate::ui::{self, Style, Text, draw_text, fill_rect, fill_round, px, rectf, stroke};
use crate::{Action, hotkey, pin};

const CLASS: PCWSTR = w!("OdileTrayMenu");
/// ピンの行のボタンのアイコン (左が「クリックをピンに戻す」、右が「閉じる」)。
const PIN_ICONS: [&str; 2] = ["clickthrough", "close"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Entry {
    Still,
    Video,
    /// ピンの段の i 本目。行そのものは押せず、ボタンだけ押せる。
    Pin(usize),
    Settings,
    Exit,
    /// `popup` の i 行目 (区切りも数える)。
    Custom(usize),
    Sep,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Item {
    entry: Entry,
    enabled: bool,
    /// ピンの行: 「クリックをピンに戻す」を押せる (透過中)。
    release: bool,
    /// オン・オフの行: 今オンか (オンなら左にチェックを出す)。
    check: Option<bool>,
}

/// `popup` の 1 行。`None` は区切り。
pub struct Row {
    pub name: &'static str,
    /// 右に出すショートカット (空なら出さない)。
    pub key: &'static str,
    /// オン・オフの行なら今の値。
    pub check: Option<bool>,
    pub enabled: bool,
}

/// 今の行。ピンの行では `btn` が今のボタン (0 = クリックをピンに戻す、1 = 閉じる)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Focus {
    row: usize,
    btn: usize,
}

/// 行の列。`pins` はピンごとのクリックを下の窓へ通しているか。ピン 0 本ならピンの段とその下の区切りを出さない。
fn items(pins: &[bool]) -> Vec<Item> {
    let it = |entry, enabled| Item {
        entry,
        enabled,
        release: false,
        check: None,
    };
    let mut v = vec![
        it(Entry::Still, true),
        it(Entry::Video, true),
        it(Entry::Sep, false),
    ];
    for (i, &clickthrough) in pins.iter().enumerate() {
        v.push(Item {
            entry: Entry::Pin(i),
            enabled: true,
            release: clickthrough,
            check: None,
        });
    }
    if !pins.is_empty() {
        v.push(it(Entry::Sep, false));
    }
    v.extend([
        it(Entry::Settings, true),
        it(Entry::Sep, false),
        it(Entry::Exit, true),
    ]);
    v
}

fn btn_on(item: &Item, btn: usize) -> bool {
    btn == 1 || item.release
}

fn focusable(items: &[Item], f: Focus) -> bool {
    let item = &items[f.row];
    item.enabled && (!matches!(item.entry, Entry::Pin(_)) || btn_on(item, f.btn))
}

/// 行と区切りの外形 (窓の中の物理 px) と窓の高さ。内側の余白 4、行 32、区切り 9、要素どうし 2。
fn layout(items: &[Item], w: i32, s: f32) -> (Vec<RECT>, i32) {
    let (pad, gap) = (px(4.0, s), px(2.0, s));
    let mut rects = Vec::with_capacity(items.len());
    let mut y = pad;
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            y += gap;
        }
        let h = px(if item.entry == Entry::Sep { 9.0 } else { 32.0 }, s);
        rects.push(RECT {
            left: pad,
            top: y,
            right: w - pad,
            bottom: y + h,
        });
        y += h;
    }
    (rects, y + pad)
}

/// 行の右に置く物。
enum Right {
    None,
    /// ホットキーの文字の幅。
    Text(f32),
    /// ピンの 2 つのボタン (右から 8 + 22 + 4 + 22)。
    Buttons,
}

/// チェックの欄の幅 (アイコン 16 + 間 8)。オン・オフの行があるメニューだけ、全部の行の名前をこれだけ右へずらす。
fn gutter(checks: bool, s: f32) -> i32 {
    if checks { px(24.0, s) } else { 0 }
}

/// 行の中身の幅: 名前の左 12、(チェックの欄)、名前、間 24、右の物、右 12 (ボタンは右から 8 の所まで)。
fn row_content(name_w: f32, right: Right, s: f32) -> f32 {
    let (p12, p24) = (px(12.0, s) as f32, px(24.0, s) as f32);
    p12 + name_w
        + match right {
            Right::None => p12,
            Right::Text(w) => p24 + w + p12,
            Right::Buttons => p24 + px(56.0, s) as f32,
        }
}

/// 窓の幅: 行の中身の最大に内側の余白 4 × 2。最小 180。
fn width(contents: &[f32], s: f32) -> i32 {
    let inner = contents.iter().copied().fold(0.0, f32::max).ceil() as i32;
    (inner + 2 * px(4.0, s)).max(px(180.0, s))
}

/// ピンの行の 2 つのボタン (22 × 22、上から 5)。右辺は行の右から 34 と 8。
fn buttons(row: RECT, s: f32) -> [RECT; 2] {
    let (top, size) = (row.top + px(5.0, s), px(22.0, s));
    let at = |inset: f32| {
        let right = row.right - px(inset, s);
        RECT {
            left: right - size,
            top,
            right,
            bottom: top + size,
        }
    };
    [at(34.0), at(8.0)]
}

/// ポインタの下の行 (窓の中の物理 px)。ピンの行はボタンの上だけ。押せるかは見ない (ツールチップは無効のボタンにも出す)。
fn hit(items: &[Item], rects: &[RECT], x: i32, y: i32, s: f32) -> Option<Focus> {
    let p = POINT { x, y };
    let row =
        (0..items.len()).find(|&i| items[i].entry != Entry::Sep && ui::contains(&rects[i], p))?;
    if !matches!(items[row].entry, Entry::Pin(_)) {
        return Some(Focus { row, btn: 0 });
    }
    let btn = buttons(rects[row], s)
        .iter()
        .position(|b| ui::contains(b, p))?;
    Some(Focus { row, btn })
}

/// ↑↓: 区切りと無効の行を飛ばし、端で反対の端へ回る。今が無ければ ↓ は先頭から、↑ は末尾から。
/// ピンの行に入ったら最初の押せるボタンを今のボタンにする。
fn step_row(items: &[Item], cur: Option<Focus>, down: bool) -> Option<Focus> {
    let n = items.len();
    (1..=n)
        .map(|k| match (cur, down) {
            (Some(f), true) => (f.row + k) % n,
            (Some(f), false) => (f.row + n - k) % n,
            (None, true) => k - 1,
            (None, false) => n - k,
        })
        .find_map(|row| {
            let item = &items[row];
            if item.entry == Entry::Sep || !item.enabled {
                return None;
            }
            let btn = match item.entry {
                Entry::Pin(_) => (0..2).find(|&b| btn_on(item, b)).unwrap_or(1),
                _ => 0,
            };
            Some(Focus { row, btn })
        })
        .or(cur)
}

/// ← →: ピンの行で隣の押せるボタンへ。動けなければそのまま。ピンの行でなければ何もしない。
fn step_btn(items: &[Item], cur: Option<Focus>, right: bool) -> Option<Focus> {
    let f = cur?;
    let item = &items[f.row];
    if !matches!(item.entry, Entry::Pin(_)) {
        return cur;
    }
    let to = if right {
        f.btn + 1
    } else {
        f.btn.wrapping_sub(1)
    };
    if to < 2 && btn_on(item, to) {
        Some(Focus { btn: to, ..f })
    } else {
        cur
    }
}

/// 窓の左上。`pt` は右クリックした点、`work` と `mon` はその点のモニタの作業領域と矩形。
/// タスクバーの辺は 2 つの矩形で違う辺。その辺を作業領域から `gap` 内側に置き、もう一方の向きは点に揃える
/// (入らなければ反対へ開く)。違う辺が無い (自動で隠す) ときは点を左下の角にして上へ開く。
/// どの場合も作業領域の端から `gap` の内側に収める。
fn place(pt: POINT, work: RECT, mon: RECT, w: i32, h: i32, gap: i32) -> POINT {
    let along_x = || {
        if pt.x + w <= work.right - gap {
            pt.x
        } else {
            pt.x - w
        }
    };
    let along_y = || {
        if pt.y + h <= work.bottom - gap {
            pt.y
        } else {
            pt.y - h
        }
    };
    let (x, y) = if work.bottom < mon.bottom {
        (along_x(), work.bottom - gap - h)
    } else if work.top > mon.top {
        (along_x(), work.top + gap)
    } else if work.left > mon.left {
        (work.left + gap, along_y())
    } else if work.right < mon.right {
        (work.right - gap - w, along_y())
    } else if pt.y - h >= work.top + gap {
        (along_x(), pt.y - h)
    } else {
        (along_x(), pt.y)
    };
    POINT {
        x: x.min(work.right - gap - w).max(work.left + gap),
        y: y.min(work.bottom - gap - h).max(work.top + gap),
    }
}

/// `popup` の窓の左上: 点を左上の角にして右下へ開く。入らない向きは反対へ開き、作業領域の端から `gap` の内側に収める。
fn place_at(pt: POINT, work: RECT, w: i32, h: i32, gap: i32) -> POINT {
    let x = if pt.x + w <= work.right - gap {
        pt.x
    } else {
        pt.x - w
    };
    let y = if pt.y + h <= work.bottom - gap {
        pt.y
    } else {
        pt.y - h
    };
    POINT {
        x: x.min(work.right - gap - w).max(work.left + gap),
        y: y.min(work.bottom - gap - h).max(work.top + gap),
    }
}

/// `popup` の行を選んだときに呼ぶ物: (渡す窓, 関数)。関数は (窓, 行の添字) を受ける。
type Pick = (HWND, fn(HWND, usize));

struct Menu {
    win: Acrylic,
    tip: Acrylic,
    s: f32,
    /// 開いた点 (右クリックした点)。ピンの操作の後に同じ所へ開き直す。
    at: POINT,
    items: Vec<Item>,
    rects: Vec<RECT>,
    names: Vec<Option<Text>>,
    keys: Vec<Option<Text>>,
    pins: Vec<HWND>,
    /// `popup` で開いたときの呼び先。
    pick: Option<Pick>,
    /// 名前をチェックの欄だけ右へずらす。
    checks: bool,
    cur: Option<Focus>,
    /// ツールチップを出しているボタン (ポインタが乗っているときだけ)。
    tip_on: Option<Focus>,
    /// 窓の左上 (画面の物理 px)。
    origin: POINT,
    work: RECT,
}

thread_local! {
    static MENU: RefCell<Option<Menu>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Menu) -> R) -> Option<R> {
    MENU.with(|c| c.try_borrow_mut().ok()?.as_mut().map(f))
}

/// トレイのアイコンの右クリック。開いていれば閉じてから開き直す。
pub fn show() {
    close();
    open(ui::cursor(), None);
}

/// `pt` (右クリックした点) に開く。開いていれば、新しい窓を出してから古い窓を消す (ピンの操作の後に中身を
/// 作り直す。消してから出すと前面が外れて一瞬消える)。`cur` は残す今の行 (押せなくなっていれば外す)。
fn open(pt: POINT, cur: Option<Focus>) {
    let t = crate::strings();
    let pins = pin::list();
    let items = items(&pins.iter().map(|p| p.3).collect::<Vec<_>>());
    let settings = crate::settings();
    let hk = crate::hotkey_texts(&settings);
    let key = |raw: &String| hotkey::format(hotkey::parse(raw).ok().flatten());
    let texts = items
        .iter()
        .map(|item| match item.entry {
            Entry::Still => Some((t.hotkey_still.to_string(), key(hk[0]))),
            Entry::Video => Some((t.hotkey_video.to_string(), key(hk[1]))),
            Entry::Pin(i) => {
                let (_, w, h, _) = pins[i];
                let name = t
                    .tray_pin
                    .replace("{n}", &(i + 1).to_string())
                    .replace("{w}", &w.to_string())
                    .replace("{h}", &h.to_string());
                Some((name, String::new()))
            }
            Entry::Settings => Some((t.menu_settings.to_string(), String::new())),
            Entry::Exit => Some((t.menu_exit.to_string(), String::new())),
            Entry::Custom(_) | Entry::Sep => None,
        })
        .collect();
    let n = pins.len();
    build(
        pt,
        items,
        texts,
        pins.iter().map(|p| p.0).collect(),
        None,
        cur,
        &format!("open ({n} pins)"),
    );
}

/// ピンの右クリックのメニュー: `pt` (右クリックした点) を左上の角にして開く。行を選ぶと、メニューを閉じてから
/// `pick(hwnd, 行の添字)` を呼ぶ (添字は区切りも数える)。外のクリックと Esc では何もせずに閉じる。
pub fn popup(pt: POINT, rows: Vec<Option<Row>>, hwnd: HWND, pick: fn(HWND, usize)) {
    close();
    let items = rows
        .iter()
        .enumerate()
        .map(|(i, r)| Item {
            entry: if r.is_some() {
                Entry::Custom(i)
            } else {
                Entry::Sep
            },
            enabled: r.as_ref().is_some_and(|r| r.enabled),
            release: false,
            check: r.as_ref().and_then(|r| r.check),
        })
        .collect();
    let texts = rows
        .iter()
        .map(|r| r.as_ref().map(|r| (r.name.to_string(), r.key.to_string())))
        .collect();
    let what = format!("popup ({} rows)", rows.len());
    build(pt, items, texts, vec![], Some((hwnd, pick)), None, &what);
}

/// 窓を作って出す。`texts` は行ごとの (名前, 右の文字)。区切りは `None`。
fn build(
    pt: POINT,
    items: Vec<Item>,
    texts: Vec<Option<(String, String)>>,
    pins: Vec<HWND>,
    pick: Option<Pick>,
    cur: Option<Focus>,
    what: &str,
) {
    widgets::register_class(CLASS, proc);
    let ui::Monitor {
        work,
        rect: mon,
        dpi,
        ..
    } = ui::monitor_at(pt);
    let s = dpi as f32 / 96.0;
    let checks = items.iter().any(|i| i.check.is_some());
    let g = gutter(checks, s) as f32;
    let (mut names, mut keys, mut contents) = (vec![], vec![], vec![]);
    for (item, text) in items.iter().zip(texts) {
        let Some((name, key)) = text else {
            names.push(None);
            keys.push(None);
            continue;
        };
        let name = ui::text(&name, Style::BodyTabular, s);
        let key = Some(key)
            .filter(|k| !k.is_empty())
            .map(|k| ui::text(&k, Style::BodyTabular, s));
        let right = match (&key, item.entry) {
            (_, Entry::Pin(_)) => Right::Buttons,
            (Some(k), _) => Right::Text(k.w),
            (None, _) => Right::None,
        };
        contents.push(row_content(g + name.w, right, s));
        names.push(Some(name));
        keys.push(key);
    }
    let w = width(&contents, s);
    let (rects, h) = layout(&items, w, s);
    let gap = px(8.0, s);
    let origin = if pick.is_some() {
        place_at(pt, work, w, h, gap)
    } else {
        place(pt, work, mon, w, h, gap)
    };
    let (mut win, tip) = match (
        Acrylic::new(CLASS, false, Capture::Setting),
        Acrylic::new(CLASS, true, Capture::Setting),
    ) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("traymenu: window failed: {e}");
            return;
        }
    };
    let hwnd = win.hwnd;
    unsafe {
        // 前面を取ってキーと外のクリック (WA_INACTIVE) を受ける (`Acrylic::new` は NOACTIVATE を付ける)。
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !(WS_EX_NOACTIVATE.0 as isize));
    }
    win.show_at(ui::rect_at(origin, w, h));
    let cur = cur.filter(|f| f.row < items.len() && focusable(&items, *f));
    let menu = Menu {
        win,
        tip,
        s,
        at: pt,
        items,
        rects,
        names,
        keys,
        pins,
        pick,
        checks,
        cur,
        tip_on: None,
        origin,
        work,
    };
    paint(&menu);
    let old = MENU.with(|c| c.borrow_mut().replace(menu));
    // 状態は先に替えたので、古い窓の WM_ACTIVATE (WA_INACTIVE) は何もしない。
    if let Some(m) = old {
        m.tip.hide_now();
        m.win.hide_now();
        unsafe {
            let _ = DestroyWindow(m.tip.hwnd);
            let _ = DestroyWindow(m.win.hwnd);
        }
    }
    crate::force_foreground(hwnd);
    let fg = unsafe { GetForegroundWindow() } == hwnd;
    eprintln!(
        "traymenu: {what} {w}x{h} at ({}, {}) scale {s}, foreground {fg}",
        origin.x, origin.y
    );
}

/// 何もせずに閉じる。開いていなければ何もしない。
fn close() -> bool {
    let Some(m) = MENU.with(|c| c.try_borrow_mut().ok().and_then(|mut c| c.take())) else {
        return false;
    };
    // 隠すと WM_ACTIVATE (WA_INACTIVE) がここへ戻るが、状態は先に取ったので何もしない。
    m.tip.hide_now();
    m.win.hide_now();
    unsafe {
        let _ = DestroyWindow(m.tip.hwnd);
        let _ = DestroyWindow(m.win.hwnd);
    }
    true
}

/// 今の行かボタンを実行する。必ずメニューを閉じてから (キャプチャのオーバーレイは前面を取る)。
/// ピンのボタン (クリックをピンに戻す・閉じる) は閉じずに、操作の後にピンの段を作り直す (続けて押せる)。
fn run(f: Focus) {
    let Some((entry, target, at, pick)) = with(|m| {
        if !focusable(&m.items, f) {
            return None;
        }
        let entry = m.items[f.row].entry;
        let target = match entry {
            Entry::Pin(i) => m.pins.get(i).copied(),
            _ => None,
        };
        Some((entry, target, m.at, m.pick))
    })
    .flatten() else {
        return;
    };
    if let Entry::Pin(i) = entry {
        let Some(hwnd) = target else { return };
        if f.btn == 0 {
            eprintln!("traymenu: pin {} release", i + 1);
            pin::release(hwnd);
        } else {
            eprintln!("traymenu: pin {} close", i + 1);
            pin::close_pin(hwnd);
        }
        open(at, Some(f));
        return;
    }
    close();
    match entry {
        Entry::Still => {
            eprintln!("traymenu: still");
            crate::dispatch(Action::Still);
        }
        Entry::Video => {
            eprintln!("traymenu: video");
            crate::dispatch(Action::Video);
        }
        Entry::Settings => {
            eprintln!("traymenu: settings");
            crate::setwin::show();
        }
        Entry::Exit => {
            eprintln!("traymenu: exit");
            unsafe {
                let _ = DestroyWindow(crate::tray_hwnd());
            }
        }
        Entry::Custom(i) => {
            if let Some((hwnd, pick)) = pick {
                pick(hwnd, i);
            }
        }
        Entry::Pin(_) | Entry::Sep => {}
    }
}

fn paint(m: &Menu) {
    let pal = ui::palette();
    let s = m.s;
    m.win.paint(pal, |ctx| {
        for (i, item) in m.items.iter().enumerate() {
            let r = m.rects[i];
            if item.entry == Entry::Sep {
                let y = (r.top + px(4.0, s)) as f32;
                let line = rectf(r.left as f32, y, r.right as f32, y + stroke(1.0, s) as f32);
                fill_rect(ctx, line, pal.separator);
                continue;
            }
            let lit = m.cur.filter(|f| f.row == i);
            if matches!(item.entry, Entry::Pin(_)) {
                // ピンの行は今の行の代わりに今のボタン (fill-hover)。
                for (b, br) in buttons(r, s).iter().enumerate() {
                    let state = if !btn_on(item, b) {
                        BtnState::Disabled
                    } else if lit.is_some_and(|f| f.btn == b) {
                        BtnState::Hover
                    } else {
                        BtnState::Normal
                    };
                    widgets::tray_icon_button(
                        ctx,
                        br.left as f32,
                        br.top as f32,
                        PIN_ICONS[b],
                        state,
                        pal,
                        s,
                    );
                }
            } else if lit.is_some() {
                fill_round(ctx, ui::rf(r), px(4.0, s) as f32, pal.fill_subtle);
            }
            let mid = |t: &Text| (r.top as f32 + ((r.bottom - r.top) as f32 - t.h) / 2.0).round();
            let ink = if item.enabled {
                pal.text_primary
            } else {
                pal.text_disabled
            };
            if item.check == Some(true) {
                let size = px(16.0, s);
                let top = r.top + (r.bottom - r.top - size) / 2;
                widgets::draw_icon(
                    ctx,
                    "check",
                    (r.left + px(12.0, s)) as f32,
                    top as f32,
                    size as f32,
                    ink,
                );
            }
            if let Some(t) = &m.names[i] {
                let x = r.left + px(12.0, s) + gutter(m.checks, s);
                draw_text(ctx, t, x as f32, mid(t), ink);
            }
            if let Some(k) = &m.keys[i] {
                let ink = if !item.enabled {
                    pal.text_disabled
                } else if lit.is_some() {
                    pal.text_primary
                } else {
                    pal.text_secondary
                };
                let x = ((r.right - px(12.0, s)) as f32 - k.w).round();
                draw_text(ctx, k, x, mid(k), ink);
            }
        }
    });
}

/// ポインタが乗っているボタンのツールチップを出す (ボタンの下 8 に中央揃え)。無ければ隠す。
fn update_tip(m: &mut Menu, on: Option<Focus>) {
    let on = on.filter(|f| matches!(m.items[f.row].entry, Entry::Pin(_)));
    if m.tip_on == on {
        return;
    }
    m.tip_on = on;
    let Some(f) = on else {
        m.tip.hide_now();
        return;
    };
    let s = m.s;
    let t = crate::strings();
    let name = if f.btn == 0 {
        t.tray_pin_release
    } else {
        t.close
    };
    let tip = Tooltip::new(name, "", s);
    let b = buttons(m.rects[f.row], s)[f.btn];
    let target = RECT {
        left: m.origin.x + b.left,
        top: m.origin.y + b.top,
        right: m.origin.x + b.right,
        bottom: m.origin.y + b.bottom,
    };
    m.tip.set_theme(ui::apps_dark());
    tip.show(&mut m.tip, target, m.work, ui::palette(), s);
}

fn is_menu(hwnd: HWND) -> bool {
    with(|m| m.win.hwnd == hwnd).unwrap_or(false)
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let def = || unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    match msg {
        WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE && is_menu(hwnd) => {
            if close() {
                eprintln!("traymenu: dismissed");
            }
            def()
        }
        WM_MOUSEMOVE | WM_MOUSELEAVE | WM_LBUTTONUP if is_menu(hwnd) => {
            // 離れの追跡は移動のときだけ入れる (KNOWLEDGE「実機検証ハーネス」の WM_MOUSELEAVE)。
            if msg == WM_MOUSEMOVE {
                ui::track_leave(hwnd);
            }
            let p = ui::lparam_point(lparam);
            let raw = with(|m| {
                if msg == WM_MOUSELEAVE {
                    None
                } else {
                    hit(&m.items, &m.rects, p.x, p.y, m.s)
                }
            })
            .flatten();
            let cur = with(|m| raw.filter(|f| focusable(&m.items, *f))).flatten();
            if msg == WM_LBUTTONUP {
                if let Some(f) = cur {
                    run(f);
                }
                return LRESULT(0);
            }
            with(|m| {
                if m.cur != cur {
                    m.cur = cur;
                    paint(m);
                }
                update_tip(m, raw);
            });
            LRESULT(0)
        }
        WM_KEYDOWN if is_menu(hwnd) => {
            let vk = wparam.0 as u16;
            if vk == VK_ESCAPE.0 {
                if close() {
                    eprintln!("traymenu: esc");
                }
            } else if vk == VK_RETURN.0 {
                if let Some(f) = with(|m| m.cur).flatten() {
                    run(f);
                }
            } else if [VK_UP.0, VK_DOWN.0, VK_LEFT.0, VK_RIGHT.0].contains(&vk) {
                with(|m| {
                    let cur = match vk {
                        v if v == VK_UP.0 => step_row(&m.items, m.cur, false),
                        v if v == VK_DOWN.0 => step_row(&m.items, m.cur, true),
                        v if v == VK_LEFT.0 => step_btn(&m.items, m.cur, false),
                        _ => step_btn(&m.items, m.cur, true),
                    };
                    if m.cur != cur {
                        m.cur = cur;
                        eprintln!("traymenu: key {vk:#x} -> {cur:?}");
                        paint(m);
                    }
                });
            }
            LRESULT(0)
        }
        _ => def(),
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

    fn entries(items: &[Item]) -> Vec<Entry> {
        items.iter().map(|i| i.entry).collect()
    }

    #[test]
    fn rows_and_heights_follow_the_card() {
        use Entry::*;
        // 100%: ピン 3 本で 277、0 本で 164 (余白 4・行 32・区切り 9・間 2)。
        let three = items(&[false, true, false]);
        assert_eq!(
            entries(&three),
            [
                Still,
                Video,
                Sep,
                Pin(0),
                Pin(1),
                Pin(2),
                Sep,
                Settings,
                Sep,
                Exit
            ]
        );
        assert_eq!(layout(&three, 251, 1.0).1, 277);
        // ピン 0 本: ピンの段とその下の区切りが無い。
        let none = items(&[]);
        assert_eq!(entries(&none), [Still, Video, Sep, Settings, Sep, Exit]);
        assert_eq!(layout(&none, 251, 1.0).1, 164);
        // 150%: 要素ごとに丸める (余白 6・行 48・区切り 14・間 3)。
        assert_eq!(layout(&three, 377, 1.5).1, 6 + 7 * 48 + 3 * 14 + 9 * 3 + 6);
        assert_eq!(layout(&none, 377, 1.5).1, 6 + 4 * 48 + 2 * 14 + 5 * 3 + 6);
        // 100% の外形: 最初の行は余白 4 の内側、最初の区切りは 2 行の 2 下。
        let (rects, _) = layout(&none, 251, 1.0);
        assert_eq!(rects[0], r(4, 4, 247, 36));
        assert_eq!(rects[2], r(4, 72, 247, 81));
        assert_eq!(rects[5], r(4, 128, 247, 160));
    }

    #[test]
    fn width_and_pin_buttons() {
        // 名前の左 12・間 24・右 12、ボタンは右から 8 + 22 + 4 + 22。最小 180。
        assert_eq!(row_content(100.0, Right::Text(80.0), 1.0), 228.0);
        assert_eq!(row_content(100.0, Right::Buttons, 1.0), 192.0);
        assert_eq!(row_content(100.0, Right::None, 1.0), 124.0);
        assert_eq!(width(&[243.0, 120.0], 1.0), 251);
        assert_eq!(width(&[100.0], 1.0), 180);
        assert_eq!(width(&[100.0], 1.5), 270);
        // 「閉じる」の右辺は行の右から 8、「クリックをピンに戻す」は 34、上から 5。
        let [release, close] = buttons(r(4, 4, 247, 36), 1.0);
        assert_eq!(close, r(217, 9, 239, 31));
        assert_eq!(release, r(191, 9, 213, 31));
    }

    #[test]
    fn hit_skips_pin_row_body_and_separators() {
        let items = items(&[true]);
        let (rects, _) = layout(&items, 251, 1.0);
        // 行 0 (静止画) の中。
        assert_eq!(
            hit(&items, &rects, 20, 20, 1.0),
            Some(Focus { row: 0, btn: 0 })
        );
        // 区切り (行 2) の上は何も無い。
        assert_eq!(hit(&items, &rects, 20, rects[2].top + 4, 1.0), None);
        // ピンの行 (行 3): 名前の上は無く、ボタンの上だけ。
        let pin = rects[3];
        assert_eq!(hit(&items, &rects, 20, pin.top + 10, 1.0), None);
        let [release, close] = buttons(pin, 1.0);
        assert_eq!(
            hit(&items, &rects, close.left + 1, close.top + 1, 1.0),
            Some(Focus { row: 3, btn: 1 })
        );
        assert_eq!(
            hit(&items, &rects, release.left + 1, release.top + 1, 1.0),
            Some(Focus { row: 3, btn: 0 })
        );
    }

    #[test]
    fn placement_table() {
        let mon = r(0, 0, 1920, 1080);
        let (w, h, gap) = (251, 311, 8);
        let p = |x, y| POINT { x, y };
        let cases = [
            // タスクバーが下: 下端 = 作業領域の下端 − 8、左端 = 点の x。
            (r(0, 0, 1920, 1032), mon, p(1500, 1050), p(1500, 713)),
            // 下で右に入らない: 右端 = 点の x。
            (r(0, 0, 1920, 1032), mon, p(1800, 1050), p(1549, 713)),
            // 上: 上端 = 作業領域の上端 + 8。
            (r(0, 48, 1920, 1080), mon, p(1500, 20), p(1500, 56)),
            // 左: 左端 = 作業領域の左端 + 8。下に入らないので上へ開く。
            (r(48, 0, 1920, 1080), mon, p(20, 900), p(56, 589)),
            // 右: 右端 = 作業領域の右端 − 8。上端 = 点の y。
            (r(0, 0, 1872, 1080), mon, p(1900, 300), p(1613, 300)),
            // 自動で隠す (違う辺が無い): 点を左下の角にして上へ開く。
            (mon, mon, p(1500, 1060), p(1500, 749)),
            // 2 枚目のモニタ (右) の下のタスクバーで右に入らない。
            (
                r(1920, 0, 3840, 1032),
                r(1920, 0, 3840, 1080),
                p(3700, 1050),
                p(3449, 713),
            ),
        ];
        for (i, (work, mon, pt, want)) in cases.into_iter().enumerate() {
            let got = place(pt, work, mon, w, h, gap);
            assert_eq!((got.x, got.y), (want.x, want.y), "case {i}");
        }
    }

    #[test]
    fn popup_opens_at_the_point() {
        // ピンのメニューは点を左上の角にして右下へ開く。タスクバーの辺には寄せない (`place` と違う)。
        let work = r(0, 0, 1920, 1032);
        let (w, h, gap) = (200, 250, 8);
        let p = |x, y| POINT { x, y };
        let cases = [
            // 入る: 点が左上。
            (p(500, 400), p(500, 400)),
            // 下に入らない: 点が左下 (上へ開く)。
            (p(500, 900), p(500, 650)),
            // 右に入らない: 点が右上 (左へ開く)。
            (p(1800, 400), p(1600, 400)),
            // 右下の隅: 左上へ開く。
            (p(1800, 900), p(1600, 650)),
            // 左上の隅の外寄り: 作業領域の端から 8 の内側に収める。
            (p(2, 2), p(8, 8)),
        ];
        for (i, (pt, want)) in cases.into_iter().enumerate() {
            let got = place_at(pt, work, w, h, gap);
            assert_eq!((got.x, got.y), (want.x, want.y), "case {i}");
        }
    }

    #[test]
    fn keys_skip_disabled_and_wrap() {
        let f = |row, btn| Some(Focus { row, btn });
        // ピン 0 本: [静止画, 動画, 区切り, 設定, 区切り, 終了]。
        let none = items(&[]);
        assert_eq!(step_row(&none, None, true), f(0, 0));
        assert_eq!(step_row(&none, None, false), f(5, 0));
        assert_eq!(step_row(&none, f(1, 0), true), f(3, 0));
        assert_eq!(step_row(&none, f(3, 0), false), f(1, 0));
        assert_eq!(step_row(&none, f(0, 0), false), f(5, 0));
        assert_eq!(step_row(&none, f(5, 0), true), f(0, 0));
        // [静止画, 動画, 区切り, ピン 1 (透過なし), ピン 2 (透過中), 区切り, 設定, 区切り, 終了]。
        let two = items(&[false, true]);
        // ピンの行に入ると最初の押せるボタン: ピン 1 は「閉じる」、ピン 2 は「クリックをピンに戻す」。
        assert_eq!(step_row(&two, f(1, 0), true), f(3, 1));
        assert_eq!(step_row(&two, f(3, 1), true), f(4, 0));
        assert_eq!(step_row(&two, f(6, 0), false), f(4, 0));
        // ← →: 無効のボタンは飛ぶ (ピン 1 の ← は動かない)。端で回らない。ピンの行でなければ何もしない。
        assert_eq!(step_btn(&two, f(3, 1), false), f(3, 1));
        assert_eq!(step_btn(&two, f(4, 0), true), f(4, 1));
        assert_eq!(step_btn(&two, f(4, 1), true), f(4, 1));
        assert_eq!(step_btn(&two, f(4, 1), false), f(4, 0));
        assert_eq!(step_btn(&two, f(6, 0), true), f(6, 0));
        assert_eq!(step_btn(&two, None, true), None);
        // 押せるかの判定: 透過していないピンの「クリックをピンに戻す」は押せない。
        assert!(!focusable(&two, Focus { row: 3, btn: 0 }));
        assert!(focusable(&two, Focus { row: 4, btn: 0 }));
    }
}
