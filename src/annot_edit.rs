//! 注釈エディタ: ピンの注釈モードの状態・描画・マウスとキー・文字の入力・ツールバーの注釈の並び。
//! ピンは `Host` を実装して、このモジュールに画像の座標・注釈の列・窓を渡す (静止画ピンが最初のホスト)。
//! ツールバーの窓 (HUD) の持ち主の処理はホストに残る。

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{EM_GETSEL, EM_SETMARGINS, EM_SETSEL};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use crate::annot::{self, Annot, Doc, P, Shape, Tool};
use crate::capture::Image;
use crate::ui::widgets::{self, BtnState, Item};
use crate::ui::{self, Palette, Style, cursor, px, rectf, stroke};

const TEXT_CLASS: PCWSTR = w!("OdileTextHost");

/// テキストの EDIT から: wparam 1 で確定、0 で取り消し。
const WM_TEXT_DONE: u32 = WM_APP + 1;

/// 注釈のツールバーの道具 (添字, 道具, アイコン): 選択 / 図形 / 文字 / 塗り。
pub const A_TOOLS: [(usize, Tool, &str); 10] = [
    (0, Tool::Select, "select"),
    (2, Tool::Arrow, "arrow"),
    (3, Tool::Line, "line"),
    (4, Tool::Rect, "rect"),
    (5, Tool::Ellipse, "ellipse"),
    (7, Tool::Text, "text"),
    (8, Tool::Badge, "badge"),
    (10, Tool::Pen, "pen"),
    (11, Tool::Highlighter, "highlighter"),
    (12, Tool::Mosaic, "mosaic"),
];
pub const A_COLOR: usize = 14;
pub const A_WIDTH: usize = 15;
pub const A_UNDO: usize = 17;
pub const A_REDO: usize = 18;
pub const A_DONE: usize = 20;

/// `opacity-annot-ghost`: 区間外の注釈の不透明度。
const GHOST_OPACITY: f32 = 0.6;

/// 注釈の見え方 (GIF ピンの区間)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vis {
    Shown,
    /// 区間外: 灰色で描き、選べる。
    Ghost,
    /// 描かず、選べない。
    Hidden,
}

/// ピンが注釈エディタに渡すもの。静止画ピンは `Pin`、GIF ピンは `GifPin` に実装する。
pub trait Host {
    /// ピンの窓: マウスのキャプチャを取り、文字の欄 (別窓) の持ち主になる。
    fn window(&self) -> HWND;
    /// ログの頭 ("pin" など)。
    fn tag(&self) -> &'static str;
    /// 画面上の画像の矩形 (物理 px、拡縮後)。
    fn img(&self) -> RECT;
    /// DPI の倍率。
    fn scale(&self) -> f32;
    /// 注釈を載せる画像 (大きさとモザイクの元)。
    fn image(&self) -> &Image;
    fn doc(&self) -> &Doc;
    /// 編集モードの間だけある。
    fn editor(&self) -> Option<&Editor>;
    /// 注釈の列と編集の状態を同時に借りる。
    fn parts(&mut self) -> (&mut Doc, Option<&mut Editor>);
    /// ピンの窓を描き直す。
    fn repaint(&self);
    /// ツールバーを描き直す (元に戻す・やり直すの可否など)。`Doc` に手を積んだ後にも呼ぶ。
    fn toolbar_changed(&self);
    /// 注釈 `i` の見え方。既定はいつも出す (静止画ピン)。
    fn vis(&self, _i: usize) -> Vis {
        Vis::Shown
    }
}

pub type HostRef = Rc<RefCell<dyn Host>>;
type WeakHost = Weak<RefCell<dyn Host>>;

/// セッションの間だけ覚える、最後に使った道具と値。
#[derive(Clone, Copy)]
pub struct Last {
    pub tool: Tool,
    pub ink: usize,
    /// 蛍光ペンの色は別に覚える (既定 `ink-yellow`)。
    pub hl_ink: usize,
    pub width: u32,
    pub size: u32,
}

impl Last {
    pub fn ink_for(&self, tool: Tool) -> usize {
        if tool == Tool::Highlighter {
            self.hl_ink
        } else {
            self.ink
        }
    }
}

pub struct Editor {
    pub tool: Tool,
    /// 選んでいる注釈 (昇順)。
    sel: Vec<usize>,
    drag: Option<Drag>,
    text: Option<TextEdit>,
    /// 直前の押下 (時刻, 位置): ダブルクリックの判定 (窓クラスは CS_DBLCLKS を持たない)。
    last_down: (u32, POINT),
}

impl Editor {
    /// 1 個だけ選んでいればその注釈 (GIF ピンの区間は 1 個ずつ)。
    pub fn sel(&self) -> Option<usize> {
        match self.sel[..] {
            [i] => Some(i),
            _ => None,
        }
    }

    /// 何か選んでいる。
    pub fn any_sel(&self) -> bool {
        !self.sel.is_empty()
    }

    /// 最後に使った道具で始める。
    pub fn new() -> Self {
        Editor {
            tool: last().tool,
            sel: Vec::new(),
            drag: None,
            text: None,
            last_down: (0, POINT::default()),
        }
    }
}

enum Drag {
    /// 描いている途中の注釈 (まだ doc に無い)。
    Draw(Annot),
    /// 選んでいる注釈を動かしている。`last` は直前の画像座標、`total` は合計。
    Move { last: P, total: P },
    /// 選択の道具で空きを引いている (画像座標の始点と今)。
    Marquee(P, P),
}

/// 入力中のテキスト: EDIT を持つ別窓をピンの上に重ねる。
struct TextEdit {
    host: HWND,
    edit: HWND,
    at: P,
    ink: usize,
    size: f32,
    /// 再編集中の注釈 (描かない)。
    replacing: Option<usize>,
    font: HFONT,
    pad: i32,
    font_px: i32,
    /// IME の変換中 (欄を広げておく)。
    composing: bool,
}

thread_local! {
    static LAST: Cell<Last> = const { Cell::new(Last {
        tool: Tool::Arrow,
        ink: annot::INK_RED,
        hl_ink: annot::INK_YELLOW,
        width: 4,
        size: 18,
    }) };
    static EDIT_PROC: Cell<isize> = const { Cell::new(0) };
    /// 開いている文字の欄 (欄の窓, ホスト)。欄の窓のプロシージャがホストを引く。
    static TEXTS: RefCell<Vec<(HWND, WeakHost)>> = const { RefCell::new(Vec::new()) };
}

pub fn last() -> Last {
    LAST.with(Cell::get)
}

fn register() {
    thread_local!(static DONE: Cell<bool> = const { Cell::new(false) });
    if !DONE.with(|d| d.replace(true)) {
        widgets::register_class(TEXT_CLASS, text_proc);
    }
}

// ---- 描画 ----

/// 注釈を画像の座標 (拡縮を変換で掛ける) で描き、選んだ注釈の枠とハンドルを画面の px で描く。
/// `local` は窓の中の画像の矩形、`opacity` は 0〜1。
pub fn draw(ctx: &ID2D1DeviceContext, h: &dyn Host, local: RECT, opacity: f32) {
    let image = rectf(
        local.left as f32,
        local.top as f32,
        local.right as f32,
        local.bottom as f32,
    );
    let (doc, src, s) = (h.doc(), h.image(), h.scale());
    let edit = h.editor();
    let skip = edit.and_then(|e| e.text.as_ref()).and_then(|t| t.replacing);
    let preview = edit.and_then(|e| match &e.drag {
        Some(Drag::Draw(a)) => Some(a),
        _ => None,
    });
    // 入力中の文字は確定後と同じ DirectWrite で描く (EDIT は透明で、入力と IME だけ受ける)。
    let live = edit.and_then(|e| e.text.as_ref()).map(|t| {
        let (text, caret) = edit_state(t.edit);
        let a = Annot {
            shape: Shape::Text {
                at: t.at,
                text,
                w: 0.0,
                h: 0.0,
            },
            ink: t.ink,
            width: 0.0,
            size: t.size,
        };
        (a, caret)
    });
    let marquee = edit.is_some_and(|e| matches!(e.drag, Some(Drag::Marquee(..))));
    if doc.items.is_empty() && preview.is_none() && live.is_none() && !marquee {
        return;
    }
    let kx = (image.right - image.left) / src.w as f32;
    let ky = (image.bottom - image.top) / src.h as f32;
    let shown = |want: Vis| {
        doc.items
            .iter()
            .enumerate()
            .filter(move |(i, _)| Some(*i) != skip && h.vis(*i) == want)
            .map(|(_, a)| a)
    };
    let ghosts: Vec<Annot> = shown(Vis::Ghost)
        .map(|a| Annot {
            ink: annot::INK_GHOST,
            ..a.clone()
        })
        .collect();
    let items = shown(Vis::Shown)
        .chain(preview)
        .chain(live.as_ref().map(|(a, _)| a));
    unsafe {
        ctx.PushAxisAlignedClip(&image, D2D1_ANTIALIAS_MODE_ALIASED);
        let m = windows_numerics::Matrix3x2::scale(kx, ky)
            * windows_numerics::Matrix3x2::translation(image.left, image.top);
        ctx.SetTransform(&m);
        if !ghosts.is_empty() {
            annot::draw(ctx, src, ghosts.iter(), opacity * GHOST_OPACITY);
        }
        annot::draw(ctx, src, items, opacity);
        ctx.SetTransform(&ui::identity());
        // カーソル: 幅 2 の縦線。ponytail: 点滅しない。
        if let Some((
            Annot {
                shape: Shape::Text { at, text, .. },
                ink,
                size,
                ..
            },
            caret,
        )) = &live
        {
            let t = ui::ink_text(text, *size);
            let x = image.left + (at.0 + t.caret_x(*caret)) * kx;
            let y = image.top + at.1 * ky;
            let w = stroke(2.0, s) as f32;
            ui::fill_rect(
                ctx,
                rectf(x - w / 2.0, y, x + w / 2.0, y + t.h * ky),
                annot::ink(*ink, 1.0),
            );
        }
        ctx.PopAxisAlignedClip();
    }
    let sel = edit
        .filter(|e| !matches!(e.drag, Some(Drag::Draw(_))))
        .map_or(&[][..], |e| &e.sel[..]);
    for a in sel
        .iter()
        .filter(|&&i| Some(i) != skip && h.vis(i) != Vis::Hidden)
        .filter_map(|&i| doc.items.get(i))
    {
        selection(ctx, a, local, src, s, true);
    }
    // 矩形選択の枠: 外接矩形がそのまま枠になるモザイクの形を借りる。
    if let Some(Drag::Marquee(p, q)) = edit.and_then(|e| e.drag.as_ref()) {
        let a = Annot {
            shape: Shape::Mosaic(*p, *q),
            ink: 0,
            width: 0.0,
            size: 0.0,
        };
        selection(ctx, &a, local, src, s, false);
    }
    // モザイクは引いている間、範囲が見えにくい: 選択と同じ破線の枠だけ出す。
    if let Some(a) = preview.filter(|a| a.is_mosaic()) {
        selection(ctx, a, local, src, s, false);
    }
}

/// 選択: 外接矩形の外に白と黒を 4 ずつの破線 (1)、四角のハンドル 8 (角と辺の中点)。線と矢印は両端に円 10。
/// `handles` が偽なら枠だけ。
fn selection(
    ctx: &ID2D1DeviceContext,
    a: &Annot,
    local: RECT,
    image: &Image,
    s: f32,
    handles: bool,
) {
    let map = |q: P| annot::to_screen(local, image.w, image.h, q);
    let (l, t, r, b) = a.bbox();
    let (sl, st) = map((l, t));
    let (sr, sb) = map((r, b));
    let fw = stroke(1.0, s) as f32;
    // 線の芯: 外接矩形の外側 1 本ぶん。
    let (fl, ft) = (sl.floor() - fw / 2.0, st.floor() - fw / 2.0);
    let (fr, fb) = (sr.ceil() + fw / 2.0, sb.ceil() + fw / 2.0);
    let frame = rectf(fl, ft, fr, fb);
    unsafe {
        let factory = &ui::gfx().factory;
        let dash = px(4.0, s) as f32 / fw;
        let Ok(style) = factory.CreateStrokeStyle(
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
        ctx.DrawRectangle(&frame, &dark, fw, None);
        ctx.DrawRectangle(&frame, &core, fw, &style);
        if !handles {
            return;
        }
        let dot = |c: P, d: f32, round: bool| {
            let h = (d / 2.0).round();
            let (x, y) = (c.0.round() - h, c.1.round() - h);
            let outer = rectf(x, y, x + d, y + d);
            let inner = rectf(x + fw, y + fw, x + d - fw, y + d - fw);
            if round {
                let e = |r: windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F| D2D1_ELLIPSE {
                    point: windows_numerics::Vector2 {
                        X: (r.left + r.right) / 2.0,
                        Y: (r.top + r.bottom) / 2.0,
                    },
                    radiusX: (r.right - r.left) / 2.0,
                    radiusY: (r.bottom - r.top) / 2.0,
                };
                ctx.FillEllipse(&e(outer), &dark);
                ctx.FillEllipse(&e(inner), &core);
            } else {
                ctx.FillRectangle(&outer, &dark);
                ctx.FillRectangle(&inner, &core);
            }
        };
        if let Some((p1, p2)) = a.ends() {
            let d = px(10.0, s) as f32;
            dot(map(p1), d, true);
            dot(map(p2), d, true);
        } else {
            let d = px(8.0, s) as f32;
            let (cx, cy) = ((fl + fr) / 2.0, (ft + fb) / 2.0);
            for c in [
                (fl, ft),
                (cx, ft),
                (fr, ft),
                (fl, cy),
                (fr, cy),
                (fl, fb),
                (cx, fb),
                (fr, fb),
            ] {
                dot(c, d, false);
            }
        }
    }
}

/// 編集モードのカーソル: 選択の道具は注釈の上で移動・ほかは矢印、文字の道具は I ビーム、ほかは十字。
pub fn cursor_id(h: &dyn Host) -> PCWSTR {
    let Some(e) = h.editor() else {
        return IDC_SIZEALL;
    };
    match e.tool {
        Tool::Select if hit(h, image_point(h, cursor())).is_some() => IDC_SIZEALL,
        Tool::Select => IDC_ARROW,
        Tool::Text => IDC_IBEAM,
        _ => IDC_CROSS,
    }
}

// ---- 操作 ----

/// 道具を替える (入力中の文字は確定する)。
pub fn set_tool(host: &RefCell<dyn Host>, tool: Tool) {
    finish_text(host, true);
    LAST.with(|l| {
        let mut v = l.get();
        v.tool = tool;
        l.set(v);
    });
    if let Some(e) = host.borrow_mut().parts().1 {
        e.tool = tool;
        // 描く道具では選択を持たない (色・線幅は次に描く物に効く)。
        if tool != Tool::Select {
            e.sel.clear();
        }
    }
    host.borrow().repaint();
}

/// 元に戻す・やり直すの前: 入力中の文字を確定し、選択を外し、ドラッグを終える
/// (移動は `Doc` に記録、描きかけは捨てる)。GIF ピンはこの後に自分の undo の列を回す。
pub fn settle(host: &RefCell<dyn Host>) {
    finish_text(host, true);
    let dragged = {
        let mut guard = host.borrow_mut();
        let (doc, edit) = guard.parts();
        match edit {
            Some(e) => {
                let sel = std::mem::take(&mut e.sel);
                match e.drag.take() {
                    Some(Drag::Move { total, .. }) => {
                        doc.moved(&sel, total);
                        true
                    }
                    Some(Drag::Draw(_) | Drag::Marquee(..)) => true,
                    None => false,
                }
            }
            None => false,
        }
    };
    if dragged {
        unsafe {
            let _ = ReleaseCapture();
        }
    }
}

/// 元に戻す・やり直す。
pub fn history(host: &RefCell<dyn Host>, redo: bool) {
    settle(host);
    let done = host.borrow_mut().parts().0.undo_redo(redo, &mut None);
    if done {
        let h = host.borrow();
        h.repaint();
        h.toolbar_changed();
        eprintln!(
            "{}: {} -> {} items",
            h.tag(),
            if redo { "redo" } else { "undo" },
            h.doc().items.len()
        );
    }
}

pub fn delete_selected(host: &RefCell<dyn Host>) {
    {
        let mut guard = host.borrow_mut();
        let (doc, edit) = guard.parts();
        let sel = edit.map(|e| std::mem::take(&mut e.sel)).unwrap_or_default();
        if sel.is_empty() {
            return;
        }
        doc.remove(&sel);
    }
    let h = host.borrow();
    h.repaint();
    h.toolbar_changed();
    eprintln!("{}: deleted, {} items", h.tag(), h.doc().items.len());
}

/// Esc: 選択を外す。外した物があれば描き直して true。
pub fn deselect(host: &RefCell<dyn Host>) -> bool {
    let had = host
        .borrow_mut()
        .parts()
        .1
        .is_some_and(|e| !std::mem::take(&mut e.sel).is_empty());
    if had {
        host.borrow().repaint();
    }
    had
}

/// 色・線幅・文字の大きさを選んだ。覚えて、選んでいる注釈にも当てる (属性の変更)。描き直しはホストが行う。
pub fn choose(host: &RefCell<dyn Host>, kind: Choice, idx: usize) {
    let tool = host.borrow().editor().map_or(Tool::Arrow, |e| e.tool);
    LAST.with(|l| {
        let mut v = l.get();
        match kind {
            Choice::Color if tool == Tool::Highlighter => v.hl_ink = idx,
            Choice::Color => v.ink = idx,
            Choice::Width => v.width = annot::WIDTHS[idx],
            Choice::Size => v.size = annot::SIZES[idx],
        }
        l.set(v);
    });
    let mut guard = host.borrow_mut();
    let (doc, edit) = guard.parts();
    let sel = edit.map(|e| e.sel.clone()).unwrap_or_default();
    let mut news = Vec::new();
    for i in sel {
        let Some(mut a) = doc.items.get(i).cloned() else {
            continue;
        };
        match kind {
            Choice::Color => a.ink = idx,
            // 文字は線幅で見た目が変わらない: 当てると何も起きない undo が積まれる。
            Choice::Width if matches!(a.shape, Shape::Text { .. }) => {}
            Choice::Width => a.width = annot::WIDTHS[idx] as f32,
            Choice::Size => {
                a.size = annot::SIZES[idx] as f32;
                if let Shape::Text { text, w, h, .. } = &mut a.shape {
                    let t = ui::ink_text(text, a.size);
                    (*w, *h) = (t.w, t.h);
                }
            }
        }
        news.push((i, a));
    }
    doc.replace_all(news);
}

/// 描く道具の新しい注釈。選択の道具は描かない (None)。
fn new_annot(tool: Tool, p: P, doc: &Doc) -> Option<Annot> {
    let l = last();
    let shape = match tool {
        Tool::Select => return None,
        Tool::Arrow => Shape::Arrow(p, p),
        Tool::Line => Shape::Line(p, p),
        Tool::Rect => Shape::Rect(p, p),
        Tool::Ellipse => Shape::Ellipse(p, p),
        Tool::Mosaic => Shape::Mosaic(p, p),
        Tool::Pen => Shape::Pen(vec![p]),
        Tool::Highlighter => Shape::Highlighter(vec![p]),
        Tool::Badge => Shape::Badge {
            at: p,
            n: doc.peek_badge(),
        },
        Tool::Text => Shape::Text {
            at: p,
            text: String::new(),
            w: 0.0,
            h: 0.0,
        },
    };
    Some(Annot {
        shape,
        ink: l.ink_for(tool),
        width: l.width as f32,
        size: l.size as f32,
    })
}

fn drag_to(a: &mut Annot, p: P) {
    match &mut a.shape {
        Shape::Arrow(_, b)
        | Shape::Line(_, b)
        | Shape::Rect(_, b)
        | Shape::Ellipse(_, b)
        | Shape::Mosaic(_, b) => *b = p,
        Shape::Pen(v) | Shape::Highlighter(v) => {
            if v.last()
                .is_none_or(|q| (q.0 - p.0).abs() + (q.1 - p.1).abs() >= 1.0)
            {
                v.push(p);
            }
        }
        Shape::Badge { at, .. } => *at = p,
        Shape::Text { .. } => {}
    }
}

fn image_point(h: &dyn Host, c: POINT) -> P {
    let image = h.image();
    annot::to_image(h.img(), image.w, image.h, (c.x as f32, c.y as f32))
}

/// 上にある物から当てる。描かない注釈 (`Vis::Hidden`) には当てない。
fn hit(h: &dyn Host, at: P) -> Option<usize> {
    h.doc().hit(at, tolerance(h), |i| h.vis(i) != Vis::Hidden)
}

/// 当たりの許し: 画面の 4 px を画像の px に。
fn tolerance(h: &dyn Host) -> f32 {
    let img = h.img();
    px(4.0, h.scale()) as f32 * h.image().w as f32 / (img.right - img.left).max(1) as f32
}

/// 編集モードの押下: 文字の入力中なら確定だけ。選択の道具は、注釈に当たれば選んで動かし、外れたら矩形選択を
/// 始める (文字のダブルクリックは再編集)。描く道具は注釈に当てず、いつも描き始める。
pub fn down(host: &HostRef) {
    if finish_text(host, true) {
        return;
    }
    let c = cursor();
    let now = unsafe { GetMessageTime() } as u32;
    let start_text = {
        let mut guard = host.borrow_mut();
        let h = &mut *guard;
        let at = image_point(h, c);
        let select = h.editor().is_some_and(|e| e.tool == Tool::Select);
        let hit = if select { hit(h, at) } else { None };
        let tag = h.tag();
        let (doc, edit) = h.parts();
        let Some(e) = edit else { return };
        let (t0, c0) = e.last_down;
        let dbl = now.wrapping_sub(t0) <= unsafe { GetDoubleClickTime() }
            && (c.x - c0.x).abs() * 2 <= unsafe { GetSystemMetrics(SM_CXDOUBLECLK) }
            && (c.y - c0.y).abs() * 2 <= unsafe { GetSystemMetrics(SM_CYDOUBLECLK) };
        e.last_down = (now, c);
        let hit_text = hit.filter(|&i| matches!(doc.items[i].shape, Shape::Text { .. }));
        if dbl && let Some(i) = hit_text {
            e.sel = vec![i];
            Some((at, Some(i)))
        } else if let Some(i) = hit {
            // 選んでいる物を押したら、選んだ物をまとめて動かす。
            if !e.sel.contains(&i) {
                eprintln!("{tag}: select {i} {}", shape_name(&doc.items[i]));
                e.sel = vec![i];
            }
            e.drag = Some(Drag::Move {
                last: at,
                total: (0.0, 0.0),
            });
            None
        } else {
            e.sel.clear();
            if e.tool == Tool::Text {
                Some((at, None))
            } else {
                e.drag = Some(match new_annot(e.tool, at, doc) {
                    Some(a) => Drag::Draw(a),
                    None => Drag::Marquee(at, at),
                });
                None
            }
        }
    };
    if let Some((at, replacing)) = start_text {
        host.borrow().repaint();
        open_text(host, at, replacing);
        return;
    }
    let hwnd = host.borrow().window();
    unsafe {
        SetCapture(hwnd);
    }
    host.borrow().repaint();
}

/// 描いている・動かしているときのマウスの移動。描き直しが要れば true。
pub fn drag(host: &RefCell<dyn Host>) -> bool {
    crate::insight_scope!("pin_edit_move");
    let c = cursor();
    let mut guard = host.borrow_mut();
    let h = &mut *guard;
    let at = image_point(h, c);
    let (doc, edit) = h.parts();
    let Some(e) = edit else {
        return false;
    };
    match &mut e.drag {
        Some(Drag::Draw(a)) => drag_to(a, at),
        Some(Drag::Move { last, total }) => {
            let d = (at.0 - last.0, at.1 - last.1);
            *last = at;
            *total = (total.0 + d.0, total.1 + d.1);
            for &i in &e.sel {
                doc.nudge(i, d);
            }
        }
        Some(Drag::Marquee(_, q)) => *q = at,
        None => return false,
    }
    true
}

/// 描き終えた・動かし終えた。
pub fn up(host: &RefCell<dyn Host>) {
    let Ok(mut guard) = host.try_borrow_mut() else {
        return;
    };
    let tag = guard.tag();
    // 矩形選択は描かない注釈 (`Vis::Hidden`) を拾わない。
    let pickable: Vec<bool> = (0..guard.doc().items.len())
        .map(|i| guard.vis(i) != Vis::Hidden)
        .collect();
    let (doc, edit) = guard.parts();
    let Some(e) = edit else {
        return;
    };
    let Some(drag) = e.drag.take() else {
        return;
    };
    match drag {
        Drag::Draw(mut a) => {
            if let Shape::Badge { n, .. } = &mut a.shape {
                *n = doc.take_badge();
            }
            if !a.degenerate() {
                // 描いた直後は選ばない: 次の色・太さは次に描く物だけに効く。
                let i = doc.add(a);
                let n = match doc.items[i].shape {
                    Shape::Badge { n, .. } => format!(" {n}"),
                    _ => String::new(),
                };
                eprintln!(
                    "{tag}: added {}{n} ({} items)",
                    shape_name(&doc.items[i]),
                    doc.items.len()
                );
            }
        }
        Drag::Move { total, .. } => {
            doc.moved(&e.sel, total);
            eprintln!("{tag}: moved {:?} by ({:.1}, {:.1})", e.sel, total.0, total.1);
        }
        // 引かずに離した空きのクリック: 外接矩形の内側で拾わない (選択は押下で外してある)。
        Drag::Marquee(p, q) if (p.0 - q.0).abs() < 2.0 && (p.1 - q.1).abs() < 2.0 => {}
        Drag::Marquee(p, q) => {
            e.sel = touching(&doc.items, p, q, &pickable);
            eprintln!("{tag}: marquee select {:?}", e.sel);
        }
    }
    drop(guard);
    let h = host.borrow();
    h.repaint();
    h.toolbar_changed();
}

/// 矩形 `p`-`q` (画像の px) に外接矩形が触れる注釈。
fn touching(items: &[Annot], p: P, q: P, pickable: &[bool]) -> Vec<usize> {
    let (l, t, r, b) = (p.0.min(q.0), p.1.min(q.1), p.0.max(q.0), p.1.max(q.1));
    items
        .iter()
        .enumerate()
        .filter(|(i, a)| {
            let (al, at, ar, ab) = a.bbox();
            pickable[*i] && al <= r && ar >= l && at <= b && ab >= t
        })
        .map(|(i, _)| i)
        .collect()
}

fn shape_name(a: &Annot) -> &'static str {
    match a.shape {
        Shape::Arrow(..) => "arrow",
        Shape::Line(..) => "line",
        Shape::Rect(..) => "rect",
        Shape::Ellipse(..) => "ellipse",
        Shape::Mosaic(..) => "mosaic",
        Shape::Pen(..) => "pen",
        Shape::Highlighter(..) => "highlighter",
        Shape::Text { .. } => "text",
        Shape::Badge { .. } => "badge",
    }
}

// ---- テキストの入力 (EDIT を重ねる) ----

/// 入力の欄を開く。`replacing` は再編集する注釈。
/// EDIT はピンの窓に重ねた別窓 (ピンが持ち主) に置く: ピンの窓は `WS_EX_NOREDIRECTIONBITMAP` なので、
/// GDI で描く子窓の面を持たない。
fn open_text(host: &HostRef, at: P, replacing: Option<usize>) {
    register();
    let (owner, tag, at, screen, k, s, text, ink, size) = {
        let h = host.borrow();
        let l = last();
        let (text, ink, size, at) = match replacing.and_then(|i| h.doc().items.get(i)) {
            Some(Annot {
                shape: Shape::Text { text, at, .. },
                ink,
                size,
                ..
            }) => (text.clone(), *ink, *size, *at),
            _ => (String::new(), l.ink, l.size as f32, at),
        };
        let (img, image) = (h.img(), h.image());
        let k = (img.right - img.left) as f32 / image.w as f32;
        let screen = annot::to_screen(img, image.w, image.h, at);
        (
            h.window(),
            h.tag(),
            at,
            screen,
            k,
            h.scale(),
            text,
            ink,
            size,
        )
    };
    let font_px = (size * k).round().max(1.0) as i32;
    let pad = px(4.0, s);
    let mut face = [0u16; 32];
    for (d, c) in face.iter_mut().zip("Segoe UI Variable Text".encode_utf16()) {
        *d = c;
    }
    let instance = unsafe { GetModuleHandleW(None).ok().map(Into::into) };
    let made = unsafe {
        let font = CreateFontIndirectW(&LOGFONTW {
            lfHeight: -font_px,
            lfWeight: 600,
            lfCharSet: DEFAULT_CHARSET,
            lfQuality: CLEARTYPE_QUALITY,
            lfFaceName: face,
            ..Default::default()
        });
        // 透明 (α 0) の窓: 見えず、クリックも下のピンへ通る。フォーカスと IME の位置だけ持つ。
        let host = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED,
            TEXT_CLASS,
            &HSTRING::new(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            Some(owner),
            None,
            instance,
            None,
        );
        let edit = host.as_ref().ok().and_then(|&host| {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                &HSTRING::from(text.as_str()),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                0,
                0,
                1,
                1,
                Some(host),
                None,
                instance,
                None,
            )
            .ok()
        });
        match (host, edit) {
            (Ok(host), Some(edit)) => Some((host, edit, font)),
            (host, _) => {
                if let Ok(h) = host {
                    let _ = DestroyWindow(h);
                }
                let _ = DeleteObject(font.into());
                None
            }
        }
    };
    let Some((text_host, edit, font)) = made else {
        eprintln!("{tag}: text edit failed");
        return;
    };
    TEXTS.with(|t| t.borrow_mut().push((text_host, Rc::downgrade(host))));
    unsafe {
        SendMessageW(
            edit,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
        // 上下 0 / 左右 4。
        let m = (pad as u32 & 0xFFFF) | ((pad as u32 & 0xFFFF) << 16);
        SendMessageW(
            edit,
            EM_SETMARGINS,
            Some(WPARAM((EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize)),
            Some(LPARAM(m as isize)),
        );
        let old = SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as isize);
        EDIT_PROC.with(|p| p.set(old));
        let _ = SetLayeredWindowAttributes(text_host, COLORREF(0), 0, LWA_ALPHA);
        ui::exclude_from_capture(text_host);
    }
    if let Some(e) = host.borrow_mut().parts().1 {
        e.text = Some(TextEdit {
            host: text_host,
            edit,
            at,
            ink,
            size,
            replacing,
            font,
            pad,
            font_px,
            composing: false,
        });
    }
    fit_text(host, screen);
    unsafe {
        let _ = ShowWindow(text_host, SW_SHOW);
        let _ = SetForegroundWindow(text_host);
        let _ = SetFocus(Some(edit));
        let n = GetWindowTextLengthW(edit) as usize;
        SendMessageW(edit, EM_SETSEL, Some(WPARAM(n)), Some(LPARAM(n as isize)));
    }
    host.borrow().repaint();
    eprintln!(
        "{tag}: text edit at ({:.0}, {:.0}) size {size} px font {font_px}",
        screen.0, screen.1
    );
}

/// 欄の大きさを文字に合わせる。`screen` は文字の左上 (画面の px)。欄はその左に 4 + 破線 1 だけ出る。
fn fit_text(host: &RefCell<dyn Host>, screen: P) {
    let Some((text_host, edit, font, pad, font_px, composing)) =
        host.borrow().editor().and_then(|e| {
            e.text
                .as_ref()
                .map(|t| (t.host, t.edit, t.font, t.pad, t.font_px, t.composing))
        })
    else {
        return;
    };
    unsafe {
        let n = GetWindowTextLengthW(edit) as usize;
        let mut buf = vec![0u16; n + 1];
        let n = GetWindowTextW(edit, &mut buf) as usize;
        let dc = GetDC(Some(edit));
        let old = SelectObject(dc, font.into());
        let mut ext = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &buf[..n], &mut ext);
        let mut tm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(dc, &mut tm);
        SelectObject(dc, old);
        ReleaseDC(Some(edit), dc);
        // 入力の先に 2 文字ぶんの余裕。IME の変換中は 16 文字ぶん (変換中の文字は欄の幅で折り返す)。
        let slack = if composing { 16 } else { 2 };
        let w = ext.cx + 2 * pad + slack * font_px;
        let h = tm.tmHeight;
        let _ = SetWindowPos(
            text_host,
            Some(HWND_TOPMOST),
            screen.0.round() as i32 - pad - 1,
            screen.1.round() as i32 - 1,
            w + 2,
            h + 2,
            SWP_NOACTIVATE,
        );
        let _ = SetWindowPos(edit, None, 1, 1, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        let _ = InvalidateRect(Some(text_host), None, true);
    }
}

/// 欄の今の位置のまま大きさを合わせ直す。`composing` は IME の変換中か。
fn refit(host: &RefCell<dyn Host>, text_host: HWND, composing: Option<bool>) {
    let pad = host.try_borrow_mut().ok().and_then(|mut h| {
        h.parts().1.and_then(|e| {
            e.text.as_mut().map(|t| {
                if let Some(c) = composing {
                    t.composing = c;
                }
                t.pad
            })
        })
    });
    if let Some(pad) = pad {
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(text_host, &mut r);
        }
        fit_text(host, ((r.left + pad + 1) as f32, (r.top + 1) as f32));
    }
}

fn forget_text(text_host: HWND) {
    TEXTS.with(|t| t.borrow_mut().retain(|x| x.0 != text_host));
}

/// 入力中の文字を閉じる。`commit` なら注釈にする (空なら追加しない・再編集なら消す)。閉じた物があれば true。
pub fn finish_text(host: &RefCell<dyn Host>, commit: bool) -> bool {
    let Some(t) = host
        .try_borrow_mut()
        .ok()
        .and_then(|mut h| h.parts().1.and_then(|e| e.text.take()))
    else {
        return false;
    };
    let (text, _) = edit_state(t.edit);
    forget_text(t.host);
    unsafe {
        let _ = DestroyWindow(t.host);
        let _ = DeleteObject(t.font.into());
    }
    if commit {
        let mut guard = host.borrow_mut();
        let tag = guard.tag();
        let (doc, edit) = guard.parts();
        if text.trim().is_empty() {
            if let Some(i) = t.replacing {
                doc.remove(&[i]);
            }
        } else {
            let m = ui::ink_text(&text, t.size);
            let a = Annot {
                shape: Shape::Text {
                    at: t.at,
                    text: text.clone(),
                    w: m.w,
                    h: m.h,
                },
                ink: t.ink,
                width: last().width as f32,
                size: t.size,
            };
            match t.replacing {
                Some(i) => doc.replace(i, a),
                None => {
                    doc.add(a);
                }
            }
        }
        // 確定した文字も選ばない (描いた後と同じ)。再編集はダブルクリックの当たり判定で選び直す。
        if let Some(e) = edit {
            e.sel.clear();
        }
        eprintln!("{tag}: text {:?}", text);
    }
    let h = host.borrow();
    h.repaint();
    h.toolbar_changed();
    true
}

/// ピンの窓が消える: 入力の欄は持ち主と一緒に消える。GDI の物だけ返す。
pub fn release_text(e: &mut Editor) {
    if let Some(t) = e.text.take() {
        forget_text(t.host);
        unsafe {
            let _ = DeleteObject(t.font.into());
        }
    }
}

/// 文字の欄の窓からホストを引く。
fn text_owner(text_host: HWND) -> Option<HostRef> {
    TEXTS.with(|t| {
        t.borrow()
            .iter()
            .find(|x| x.0 == text_host)
            .and_then(|x| x.1.upgrade())
    })
}

unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let text_host = unsafe { GetParent(hwnd) }.unwrap_or_default();
    match msg {
        WM_KEYDOWN if wparam.0 as u16 == VK_RETURN.0 || wparam.0 as u16 == VK_ESCAPE.0 => {
            let commit = (wparam.0 as u16 == VK_RETURN.0) as usize;
            unsafe {
                let _ = PostMessageW(Some(text_host), WM_TEXT_DONE, WPARAM(commit), LPARAM(0));
            }
            LRESULT(0)
        }
        // 1 行の EDIT は Enter と Esc で鳴る。
        WM_CHAR if wparam.0 == 0x0D || wparam.0 == 0x1B => LRESULT(0),
        WM_IME_STARTCOMPOSITION | WM_IME_ENDCOMPOSITION => unsafe {
            let old: WNDPROC = std::mem::transmute(EDIT_PROC.with(Cell::get));
            let r = CallWindowProcW(old, hwnd, msg, wparam, lparam);
            if let Some(h) = text_owner(text_host) {
                refit(&h, text_host, Some(msg == WM_IME_STARTCOMPOSITION));
            }
            r
        },
        // 矢印・Home・End でカーソルが動く: 描き直す。
        WM_KEYDOWN => unsafe {
            let old: WNDPROC = std::mem::transmute(EDIT_PROC.with(Cell::get));
            let r = CallWindowProcW(old, hwnd, msg, wparam, lparam);
            repaint(text_host);
            r
        },
        _ => unsafe {
            let old: WNDPROC = std::mem::transmute(EDIT_PROC.with(Cell::get));
            CallWindowProcW(old, hwnd, msg, wparam, lparam)
        },
    }
}

unsafe extern "system" fn text_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TEXT_DONE => {
            if let Some(h) = text_owner(hwnd) {
                finish_text(&h, wparam.0 == 1);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match (wparam.0 >> 16) as u32 {
                EN_KILLFOCUS => unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_TEXT_DONE, WPARAM(1), LPARAM(0));
                },
                EN_CHANGE => {
                    if let Some(h) = text_owner(hwnd) {
                        refit(&h, hwnd, None);
                    }
                    repaint(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// EDIT の文字とカーソルの位置 (UTF-16)。
fn edit_state(edit: HWND) -> (String, u32) {
    unsafe {
        let n = GetWindowTextLengthW(edit) as usize;
        let mut buf = vec![0u16; n + 1];
        let n = GetWindowTextW(edit, &mut buf) as usize;
        // 下位が選択の始め、上位が終わり。カーソルは終わりに置く。
        let sel = SendMessageW(edit, EM_GETSEL, None, None).0 as u32;
        (String::from_utf16_lossy(&buf[..n]), sel >> 16)
    }
}

/// 欄の持ち主のピンを描き直す (借りられなければ何もしない)。
fn repaint(text_host: HWND) {
    if let Some(h) = text_owner(text_host)
        && let Ok(h) = h.try_borrow()
    {
        h.repaint();
    }
}

// ---- ツールバーの注釈の並びとポップアップ ----

/// 注釈のツールバーのポップアップ。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    Color,
    Width,
    Size,
}

/// 注釈のツールバーのボタンを押した。
pub enum Action {
    Tool(Tool),
    Popup(Choice),
    Undo,
    Redo,
    Done,
}

/// 図形 / 文字 / 塗り / 色、線幅 / 元に戻す、やり直す / 完了。完了は `check` のツールのボタン (反転した面なし)。
pub fn items() -> Vec<Item> {
    let mut items = vec![Item::Sep; A_DONE + 1];
    for (i, _, icon) in A_TOOLS {
        items[i] = Item::Tool(icon);
    }
    items[A_COLOR] = Item::Slot;
    items[A_WIDTH] = Item::Slot;
    items[A_UNDO] = Item::Tool("undo");
    items[A_REDO] = Item::Tool("redo");
    items[A_DONE] = Item::Tool("check");
    items
}

fn tool_at(i: usize) -> Option<Tool> {
    A_TOOLS.iter().find(|t| t.0 == i).map(|t| t.1)
}

/// ボタン `i` の働き (`tool` は今の道具: 線幅のボタンは文字の道具で文字の大きさになる)。
pub fn action(i: usize, tool: Tool) -> Option<Action> {
    if let Some(t) = tool_at(i) {
        return Some(Action::Tool(t));
    }
    match i {
        A_COLOR => Some(Action::Popup(Choice::Color)),
        A_WIDTH if tool == Tool::Text => Some(Action::Popup(Choice::Size)),
        A_WIDTH => Some(Action::Popup(Choice::Width)),
        A_UNDO => Some(Action::Undo),
        A_REDO => Some(Action::Redo),
        A_DONE => Some(Action::Done),
        _ => None,
    }
}

/// ボタン `i` の (オン, 無効, ポップアップが開いている)。
pub fn button(
    i: usize,
    tool: Tool,
    undo: bool,
    redo: bool,
    open: Option<Choice>,
) -> (bool, bool, bool) {
    (
        tool_at(i) == Some(tool),
        (i == A_UNDO && !undo) || (i == A_REDO && !redo),
        (i == A_COLOR && open == Some(Choice::Color))
            || (i == A_WIDTH && matches!(open, Some(Choice::Width | Choice::Size))),
    )
}

/// ツールチップの (名前, キー)。
pub fn tip(i: usize, tool: Tool) -> (&'static str, &'static str) {
    let t = crate::strings();
    if let Some(k) = A_TOOLS.iter().position(|a| a.0 == i) {
        return (t.tools[k], "");
    }
    match i {
        A_COLOR => (t.color, ""),
        A_WIDTH if tool == Tool::Text => (t.text_size, ""),
        A_WIDTH => (t.line_width, ""),
        A_UNDO => (t.undo, "Ctrl+Z"),
        A_REDO => (t.redo, "Ctrl+Y"),
        A_DONE => (t.done, ""),
        _ => ("", ""),
    }
}

fn ink_color(i: usize) -> D2D1_COLOR_F {
    ui::hex(annot::INKS[i])
}

/// 色と線幅 (文字の道具では文字の大きさ) の枠の中身。`layout` は `toolbar_layout` の結果。
pub fn draw_slots(
    ctx: &ID2D1DeviceContext,
    layout: &[(i32, i32)],
    tool: Tool,
    color: BtnState,
    width: BtnState,
    pal: &Palette,
    s: f32,
) {
    let l = last();
    let top = px(4.0, s) as f32;
    let (cx, _) = layout[A_COLOR];
    widgets::swatch(
        ctx,
        cx as f32,
        top,
        ink_color(l.ink_for(tool)),
        color,
        false,
        pal,
        s,
    );
    let (wx, bw) = layout[A_WIDTH];
    let (wx, b) = (wx as f32, bw as f32);
    let back = match width {
        BtnState::Hover => Some(pal.fill_hover),
        BtnState::Pressed | BtnState::Open => Some(pal.fill_pressed),
        _ => None,
    };
    if let Some(c) = back {
        ui::fill_round(ctx, rectf(wx, top, wx + b, top + b), px(4.0, s) as f32, c);
    }
    if tool == Tool::Text {
        let t = ui::text(&l.size.to_string(), Style::NumStrong, s);
        ui::draw_text(
            ctx,
            &t,
            (wx + (b - t.w) / 2.0).round(),
            (top + (b - t.h) / 2.0).round(),
            pal.text_primary,
        );
    } else {
        widgets::width_sample(
            ctx,
            wx + b / 2.0,
            top + b / 2.0,
            l.width as f32,
            pal.text_primary,
            s,
        );
    }
}

/// ポップアップの (開いたボタンの添字, 幅, 高さ)。
pub fn popup_geometry(kind: Choice, s: f32) -> (usize, i32, i32) {
    match kind {
        Choice::Color => (
            A_COLOR,
            px(widgets::COLOR_POPUP_W, s),
            widgets::choice_popup_h(s),
        ),
        Choice::Width => (
            A_WIDTH,
            px(widgets::WIDTH_POPUP_W, s),
            widgets::choice_popup_h(s),
        ),
        Choice::Size => (
            A_WIDTH,
            px(widgets::SIZE_POPUP_W, s),
            widgets::choice_popup_h(s),
        ),
    }
}

/// 色・線幅・文字の大きさのポップアップの当たり (外形から)。
pub fn popup_hit(kind: Choice, x: i32, y: i32, s: f32) -> Option<usize> {
    match kind {
        Choice::Color => widgets::choice_hit(annot::INKS.len(), x, y, s),
        Choice::Width => widgets::choice_hit(annot::WIDTHS.len(), x, y, s),
        Choice::Size => widgets::seg_hit(annot::SIZES.len(), widgets::SIZE_POPUP_W, x, y, s),
    }
}

/// 色・線幅・文字の大きさのポップアップの中身。`pw` はポップアップの幅。
pub fn draw_popup(
    ctx: &ID2D1DeviceContext,
    kind: Choice,
    tool: Tool,
    hover: Option<usize>,
    pw: f32,
    pal: &Palette,
    s: f32,
) {
    let strings = crate::strings();
    let l = last();
    match kind {
        Choice::Color => {
            let sel = l.ink_for(tool);
            widgets::popup_head(ctx, strings.color, strings.inks[sel], pw, pal, s);
            let colors: Vec<_> = (0..annot::INKS.len()).map(ink_color).collect();
            widgets::draw_color_choices(ctx, &colors, sel, hover, pal, s);
        }
        Choice::Width => {
            let sel = annot::WIDTHS
                .iter()
                .position(|&w| w == l.width)
                .unwrap_or(1);
            widgets::popup_head(
                ctx,
                strings.line_width,
                &format!("{} px", l.width),
                pw,
                pal,
                s,
            );
            widgets::draw_width_choices(ctx, &annot::WIDTHS, sel, hover, pal, s);
        }
        Choice::Size => {
            let sel = annot::SIZES.iter().position(|&v| v == l.size).unwrap_or(1);
            widgets::popup_head(
                ctx,
                strings.text_size,
                &format!("{} px", l.size),
                pw,
                pal,
                s,
            );
            let labels: Vec<String> = annot::SIZES.iter().map(|v| v.to_string()).collect();
            widgets::draw_seg(ctx, &labels, sel, hover, widgets::SIZE_POPUP_W, pal, s);
        }
    }
}
