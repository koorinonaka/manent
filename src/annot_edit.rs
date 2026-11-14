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
use windows::Win32::UI::Controls::{EM_GETSEL, EM_REPLACESEL, EM_SETMARGINS, EM_SETSEL};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_BACK, VK_CONTROL,
    VK_ESCAPE, VK_RETURN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use crate::annot::{self, Annot, Deco, Doc, P, Shape, Tool};
use crate::capture::Image;
use crate::ui::widgets::{self, BtnState, Item};
use crate::ui::{self, Palette, Style, cursor, px, rectf, stroke};

const TEXT_CLASS: PCWSTR = w!("OdileTextHost");

/// テキストの EDIT から: wparam 1 で確定、0 で取り消し。
const WM_TEXT_DONE: u32 = WM_APP + 1;
/// 文字の欄のカーソルの点滅 (欄の窓のタイマー)。
const BLINK_TIMER: usize = 1;

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
/// 文字の装飾 (なし・縁取り・背景)。
pub const A_DECO: usize = 16;
pub const A_UNDO: usize = 18;
pub const A_REDO: usize = 19;
pub const A_DONE: usize = 21;

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
    /// モザイクの升の bitmap の置き場と、`image` の中身の番号 (静止画は 0、GIF はフレーム)。
    fn mosaics(&self) -> (&annot::MosaicCache, usize);
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
    /// 次に置くテキストの装飾。
    pub deco: Deco,
    /// 次に描く四角・丸を塗るか。
    pub fill: bool,
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
    /// 1 個だけ選んでいればその注釈。
    pub fn sel(&self) -> Option<usize> {
        match self.sel[..] {
            [i] => Some(i),
            _ => None,
        }
    }

    /// 選んでいる注釈 (昇順)。
    pub fn sels(&self) -> &[usize] {
        &self.sel
    }

    /// 何か選んでいる。
    pub fn any_sel(&self) -> bool {
        !self.sel.is_empty()
    }

    /// 最後に使った道具で始める (選択の道具なら矢印)。
    pub fn new() -> Self {
        Editor {
            // 選択の道具で終えていても、開いたら描く道具 (矢印) から: 開いた直後は選ぶ物が無い。
            tool: match last().tool {
                Tool::Select => Tool::Arrow,
                t => t,
            },
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
    /// 1 個だけ選んだ注釈 `i` のハンドル `handle` を引いている。`orig` は押したときの形、`start` は押した画像座標。
    Resize {
        i: usize,
        handle: usize,
        orig: Annot,
        start: P,
    },
}

/// 入力中のテキスト: EDIT を持つ別窓をピンの上に重ねる。
struct TextEdit {
    host: HWND,
    edit: HWND,
    at: P,
    ink: usize,
    size: f32,
    /// 付ける装飾 (入力中も描く)。
    deco: Deco,
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
        fill: false,
        deco: Deco::None,
    }) };
    static EDIT_PROC: Cell<isize> = const { Cell::new(0) };
    /// 文字の欄のカーソルを今出しているか (点滅)。ponytail: 欄はフォーカスのある 1 つだけが点滅するので 1 つで足りる。
    static CARET_ON: Cell<bool> = const { Cell::new(true) };
    /// 開いている文字の欄 (欄の窓, ホスト)。欄の窓のプロシージャがホストを引く。
    static TEXTS: RefCell<Vec<(HWND, WeakHost)>> = const { RefCell::new(Vec::new()) };
    /// 注釈のコピー (アプリの中だけ。ピンをまたいで貼れる)。貼るたびにずらして持ち直す。
    static CLIP: RefCell<Vec<Annot>> = const { RefCell::new(Vec::new()) };
}

pub fn last() -> Last {
    LAST.with(Cell::get)
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
        let (text, sel) = edit_state(t.edit);
        // 装飾も入力中から出す (確定後と同じ見た目)。寸法は今の文字の箱で測る。空のうちは出さない。
        let m = ui::ink_text(&text, t.size);
        let deco = if text.is_empty() { Deco::None } else { t.deco };
        let a = Annot {
            shape: Shape::Text {
                at: t.at,
                text,
                w: m.w,
                h: m.h,
                deco,
            },
            ink: t.ink,
            width: 0.0,
            size: t.size,
            fill: false,
        };
        (a, sel)
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
            annot::draw(
                ctx,
                src,
                h.mosaics(),
                ghosts.iter(),
                opacity * GHOST_OPACITY,
            );
        }
        annot::draw(ctx, src, h.mosaics(), items, opacity);
        ctx.SetTransform(&ui::identity());
        // 選択の地と、カーソル (幅 2 の縦線、点滅)。カーソルは選択の終わりに置く。
        if let Some((
            Annot {
                shape: Shape::Text { at, text, .. },
                ink,
                size,
                ..
            },
            (a, b),
        )) = &live
        {
            let t = ui::ink_text(text, *size);
            let y = image.top + at.1 * ky;
            let y2 = y + t.h * ky;
            if a != b {
                let xa = image.left + (at.0 + t.caret_x(*a.min(b))) * kx;
                let xb = image.left + (at.0 + t.caret_x(*a.max(b))) * kx;
                ui::fill_rect(ctx, rectf(xa, y, xb, y2), ui::rgba(0x33, 0x99, 0xFF, 0.35));
            }
            if CARET_ON.with(Cell::get) {
                let x = image.left + (at.0 + t.caret_x(*b)) * kx;
                let w = stroke(2.0, s) as f32;
                ui::fill_rect(
                    ctx,
                    rectf(x - w / 2.0, y, x + w / 2.0, y2),
                    annot::ink(*ink, 1.0),
                );
            }
        }
        ctx.PopAxisAlignedClip();
    }
    // 矩形選択を引いている間は、離したら選ぶ注釈を選択の枠で出す。
    let picking;
    let sel = match edit {
        Some(Editor {
            drag: Some(Drag::Marquee(p, q)),
            sel,
            ..
        }) => {
            let pickable: Vec<bool> = (0..doc.items.len())
                .map(|i| h.vis(i) != Vis::Hidden)
                .collect();
            picking = marquee_pick(&doc.items, *p, *q, &pickable, sel);
            &picking[..]
        }
        Some(e) if !matches!(e.drag, Some(Drag::Draw(_))) => &e.sel[..],
        _ => &[][..],
    };
    for a in sel
        .iter()
        .filter(|&&i| Some(i) != skip && h.vis(i) != Vis::Hidden)
        .filter_map(|&i| doc.items.get(i))
    {
        // ハンドルは 1 個だけ選んだときだけ (大きさを変えられるのはそのときだけ)。
        selection(ctx, a, local, src, s, sel.len() == 1 && !marquee);
    }
    // 矩形選択の枠: 外接矩形がそのまま枠になるモザイクの形を借りる。
    if let Some(Drag::Marquee(p, q)) = edit.and_then(|e| e.drag.as_ref()) {
        let a = Annot {
            shape: Shape::Mosaic(*p, *q),
            ink: 0,
            width: 0.0,
            size: 0.0,
            fill: false,
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
    let fw = stroke(1.0, s) as f32;
    let (fl, ft, fr, fb) = frame(a, local, image, s);
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
        let (pts, d) = grips(a, local, image, s);
        for c in pts {
            dot(c, d, a.ends().is_some());
        }
    }
}

/// 選択の枠の線の芯 (左, 上, 右, 下): `rect` に画像を出したときの px で、外接矩形の外側 1 本ぶん。
fn frame(a: &Annot, rect: RECT, image: &Image, s: f32) -> (f32, f32, f32, f32) {
    let map = |q: P| annot::to_screen(rect, image.w, image.h, q);
    let (l, t, r, b) = a.bbox();
    let (sl, st) = map((l, t));
    let (sr, sb) = map((r, b));
    let fw = stroke(1.0, s) as f32;
    (
        sl.floor() - fw / 2.0,
        st.floor() - fw / 2.0,
        sr.ceil() + fw / 2.0,
        sb.ceil() + fw / 2.0,
    )
}

/// 箱のハンドルが動かす辺 (横, 縦): -1 は左・上、1 は右・下、0 は動かさない。`grips` の並びと同じ。
const SIDES: [(i8, i8); 8] = [
    (-1, -1),
    (0, -1),
    (1, -1),
    (-1, 0),
    (1, 0),
    (-1, 1),
    (0, 1),
    (1, 1),
];

/// ハンドルの中心 (`rect` に画像を出したときの px) と直径。線と矢印は両端の円 10、大きさを変えられる形は
/// 角と辺の中点の四角 8 (`SIDES` の順)。文字とバッジは大きさを変えないので無し。
fn grips(a: &Annot, rect: RECT, image: &Image, s: f32) -> (Vec<P>, f32) {
    if let Some((p1, p2)) = a.ends() {
        let map = |q: P| annot::to_screen(rect, image.w, image.h, q);
        return (vec![map(p1), map(p2)], px(10.0, s) as f32);
    }
    if a.core_box().is_none() {
        return (Vec::new(), 0.0);
    }
    let (l, t, r, b) = frame(a, rect, image, s);
    let pick = |k: i8, lo: f32, hi: f32| match k {
        -1 => lo,
        1 => hi,
        _ => (lo + hi) / 2.0,
    };
    let pts = SIDES
        .iter()
        .map(|&(x, y)| (pick(x, l, r), pick(y, t, b)))
        .collect();
    (pts, px(8.0, s) as f32)
}

/// カーソルの下のハンドル (注釈, ハンドル)。選択の道具で 1 個だけ選んでいるときだけ。
fn handle_at(h: &dyn Host, c: POINT) -> Option<(usize, usize)> {
    let e = h.editor()?;
    let i = e.sel().filter(|_| e.tool == Tool::Select)?;
    if h.vis(i) == Vis::Hidden {
        return None;
    }
    let a = h.doc().items.get(i)?;
    let (pts, d) = grips(a, h.img(), h.image(), h.scale());
    let reach = d / 2.0 + px(2.0, h.scale()) as f32;
    pts.iter()
        .position(|p| (p.0 - c.x as f32).abs() <= reach && (p.1 - c.y as f32).abs() <= reach)
        .map(|k| (i, k))
}

/// Shift の正規化: `p` を `anchor` から見て、線と矢印 (`ortho`) は水平・垂直・45° のうち近い向きへ
/// (境目は 22.5°)、箱は正方形 (長い辺に揃える) へ寄せる。
fn constrain(anchor: P, p: P, ortho: bool) -> P {
    let (dx, dy) = (p.0 - anchor.0, p.1 - anchor.1);
    let tan = std::f32::consts::FRAC_PI_8.tan();
    if ortho && dy.abs() <= dx.abs() * tan {
        (p.0, anchor.1)
    } else if ortho && dx.abs() <= dy.abs() * tan {
        (anchor.0, p.1)
    } else {
        // 正方形の角は 45° の線の端でもある。
        let side = dx.abs().max(dy.abs());
        (anchor.0 + side * dx.signum(), anchor.1 + side * dy.signum())
    }
}

/// Shift を押しているか。
fn shift() -> bool {
    unsafe { GetKeyState(VK_SHIFT.0 as i32) < 0 }
}

/// ハンドル `handle` を画像の px で `d` だけ引いた形。`square` (Shift) なら線は水平・垂直・45°、
/// 四角・丸・モザイクの角は正方形に揃える (辺のハンドルは揃えない)。
fn resized(orig: &Annot, handle: usize, d: P, square: bool) -> Annot {
    let mut a = orig.clone();
    if let Shape::Arrow(p, q) | Shape::Line(p, q) = &mut a.shape {
        let (e, other) = if handle == 0 { (p, *q) } else { (q, *p) };
        *e = (e.0 + d.0, e.1 + d.1);
        if square {
            *e = constrain(other, *e, true);
        }
    } else if let Some(from) = orig.core_box() {
        let (sx, sy) = SIDES[handle];
        let boxy = matches!(
            orig.shape,
            Shape::Rect(..) | Shape::Ellipse(..) | Shape::Mosaic(..)
        );
        let d = if square && boxy && sx != 0 && sy != 0 {
            let pick = |k: i8, lo: f32, hi: f32| if k < 0 { (lo, hi) } else { (hi, lo) };
            let (cx, ax) = pick(sx, from.0, from.2);
            let (cy, ay) = pick(sy, from.1, from.3);
            let c = constrain((ax, ay), (cx + d.0, cy + d.1), false);
            (c.0 - cx, c.1 - cy)
        } else {
            d
        };
        let edge = |k: i8, side: i8, v: f32, dv: f32| if k == side { v + dv } else { v };
        let to = (
            edge(sx, -1, from.0, d.0),
            edge(sy, -1, from.1, d.1),
            edge(sx, 1, from.2, d.0),
            edge(sy, 1, from.3, d.1),
        );
        a.fit(from, to);
    }
    a
}

/// 引き終えた形を 1 手 (属性の変更) にする。潰れた形は押したときの形に戻す。
fn commit_resize(doc: &mut Doc, i: usize, orig: Annot) {
    let Some(now) = doc.items.get_mut(i) else {
        return;
    };
    let new = std::mem::replace(now, orig);
    if !new.degenerate() {
        doc.replace(i, new);
    }
}

/// 編集モードのカーソル: 選択の道具は注釈の上で移動・ほかは矢印、文字の道具は I ビーム、ほかは十字。
pub fn cursor_id(h: &dyn Host) -> PCWSTR {
    let Some(e) = h.editor() else {
        return IDC_SIZEALL;
    };
    let c = cursor();
    if let Some((i, k)) = handle_at(h, c) {
        if h.doc().items[i].ends().is_some() {
            return IDC_CROSS;
        }
        return match SIDES[k] {
            (0, _) => IDC_SIZENS,
            (_, 0) => IDC_SIZEWE,
            (x, y) if x == y => IDC_SIZENWSE,
            _ => IDC_SIZENESW,
        };
    }
    match e.tool {
        Tool::Select if hit(h, image_point(h, c)).is_some() => IDC_SIZEALL,
        Tool::Select => IDC_ARROW,
        Tool::Text => IDC_IBEAM,
        Tool::Badge => IDC_HAND,
        _ => IDC_CROSS,
    }
}

/// 注釈のツールバーの `i` 番目の上のカーソル: 押せるボタンは指、押せないボタン (元に戻すが空など) と隙間は矢印。
pub fn bar_cursor(h: &dyn Host, i: Option<usize>) -> PCWSTR {
    let Some(e) = h.editor() else {
        return IDC_ARROW;
    };
    let (undo, redo) = (h.doc().can_undo(), h.doc().can_redo());
    let sh = shown(h);
    let pressable = |i: usize| !button(i, e.tool, undo, redo, &sh, None).1;
    if i.is_some_and(pressable) {
        IDC_HAND
    } else {
        IDC_ARROW
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
        edit.is_some_and(|e| take_sel(doc, e).1)
    };
    if dragged {
        release_drag();
    }
}

/// 選択を外す。その前にドラッグを終える (移動とハンドルの拡縮は `Doc` に記録、描きかけは捨てる)。
/// 終えないと、離したときの `up` が空の選択で移動を記録し (undo が元へ戻さない)、
/// 消した後の拡縮は古い添字に当たる。→ (外した選択, ドラッグを終えたか)。
fn take_sel(doc: &mut Doc, e: &mut Editor) -> (Vec<usize>, bool) {
    let dragged = match e.drag.take() {
        Some(Drag::Move { total, .. }) => {
            doc.moved(&e.sel, total);
            true
        }
        Some(Drag::Resize { i, orig, .. }) => {
            commit_resize(doc, i, orig);
            true
        }
        Some(Drag::Draw(_) | Drag::Marquee(..)) => true,
        None => false,
    };
    (std::mem::take(&mut e.sel), dragged)
}

/// ドラッグを終えた後の捕捉を返す。借用の外で呼ぶ (WM_CAPTURECHANGED が同期で `up` に入る)。
fn release_drag() {
    unsafe {
        let _ = ReleaseCapture();
    }
}

/// 元に戻す・やり直す。
pub fn history(host: &RefCell<dyn Host>, redo: bool) {
    settle(host);
    let done = host.borrow_mut().parts().0.undo_redo(redo);
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
    let Some(dragged) = drop_sel(host, true) else {
        return;
    };
    if dragged {
        release_drag();
    }
    let h = host.borrow();
    h.repaint();
    h.toolbar_changed();
    eprintln!("{}: deleted, {} items", h.tag(), h.doc().items.len());
}

/// Ctrl+C: 選んでいる注釈を覚える。選んでいなければ false (ホストは画像をコピーする)。
pub fn copy_selected(host: &RefCell<dyn Host>) -> bool {
    let h = host.borrow();
    let Some(e) = h.editor().filter(|e| e.any_sel()) else {
        return false;
    };
    let items: Vec<Annot> = e
        .sel
        .iter()
        .filter_map(|&i| h.doc().items.get(i).cloned())
        .collect();
    eprintln!("{}: copied {} annots", h.tag(), items.len());
    CLIP.with(|c| *c.borrow_mut() = items);
    true
}

/// Ctrl+V: 覚えた注釈を 1 手で足し、選択の道具で選ぶ。覚えた物が無ければ false。
/// カーソルが画像の上なら、覚えた物の外接矩形の中心をカーソルに置く。外なら少しずらして貼る。
pub fn paste(host: &RefCell<dyn Host>) -> bool {
    if CLIP.with(|c| c.borrow().is_empty()) {
        return false;
    }
    set_tool(host, Tool::Select);
    settle(host);
    let mut guard = host.borrow_mut();
    let img = guard.img();
    let c = cursor();
    let at = (c.x >= img.left && c.x < img.right && c.y >= img.top && c.y < img.bottom)
        .then(|| image_point(&*guard, c));
    // カーソルが画像の外: 画面の 16 px ぶん右下へ。ponytail: 画像の外へ出ても戻さない。小さいピンへ貼ると見えないことがある。
    let k = guard.image().w as f32 / (img.right - img.left).max(1) as f32;
    let d = px(16.0, guard.scale()) as f32 * k;
    let tag = guard.tag();
    let (doc, edit) = guard.parts();
    let added = CLIP.with(|c| {
        let mut clip = c.borrow_mut();
        match at {
            // カーソルへ貼るときは覚えた物を動かさない (続けて貼っても階段にしない)。
            Some(p) => pasted(&mut clip.clone(), to_cursor(&clip, p), doc),
            None => pasted(&mut clip, (d, d), doc),
        }
    });
    eprintln!("{tag}: pasted {added:?}");
    if let Some(e) = edit {
        e.sel = added.collect();
    }
    drop(guard);
    let h = host.borrow();
    h.repaint();
    h.toolbar_changed();
    true
}

/// `clip` 全体の外接矩形の中心を `at` へ動かすずれ。
fn to_cursor(clip: &[Annot], at: P) -> P {
    let (l, t, r, b) = clip
        .iter()
        .map(Annot::bbox)
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))
        .unwrap_or_default();
    (at.0 - (l + r) / 2.0, at.1 - (t + b) / 2.0)
}

/// `clip` を `d` だけずらして持ち直し、`doc` に 1 手で足す (続けて貼ると階段状に並ぶ)。バッジは番号を振り直す。
fn pasted(clip: &mut [Annot], d: P, doc: &mut Doc) -> std::ops::Range<usize> {
    let mut v = Vec::with_capacity(clip.len());
    for a in clip.iter_mut() {
        a.translate(d);
        let mut b = a.clone();
        if let Shape::Badge { n, .. } = &mut b.shape {
            *n = doc.take_badge();
        }
        v.push(b);
    }
    doc.add_all(v)
}

/// Esc: 選択を外す。外した物があれば描き直して true。
pub fn deselect(host: &RefCell<dyn Host>) -> bool {
    let Some(dragged) = drop_sel(host, false) else {
        return false;
    };
    if dragged {
        release_drag();
    }
    let h = host.borrow();
    h.repaint();
    // 文字の装飾のボタンは、テキストを選んでいる間だけ押せる。
    h.toolbar_changed();
    true
}

/// 選んでいる物があれば、ドラッグを終えて選択を外す (`remove` なら消す)。→ ドラッグを終えたか。
/// 何も選んでいなければ None (描きかけには触れない)。
fn drop_sel(host: &RefCell<dyn Host>, remove: bool) -> Option<bool> {
    let mut guard = host.borrow_mut();
    let (doc, edit) = guard.parts();
    let e = edit.filter(|e| e.any_sel())?;
    let (sel, dragged) = take_sel(doc, e);
    if remove {
        doc.remove(&sel);
    }
    Some(dragged)
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
            Choice::Deco => v.deco = annot::DECOS[idx],
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
            // 装飾はテキストだけ。ほかの注釈は変えない (変わらない物は手を積まない)。
            Choice::Deco => {
                if let Shape::Text { deco, .. } = &mut a.shape {
                    *deco = annot::DECOS[idx];
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
            deco: l.deco,
        },
    };
    Some(Annot {
        shape,
        ink: l.ink_for(tool),
        width: l.width as f32,
        size: l.size as f32,
        fill: l.fill,
    })
}

/// 描きかけを `p` まで伸ばす。`square` (Shift) なら線と矢印は水平・垂直・45°、四角・丸・モザイクは正方形。
fn drag_to(a: &mut Annot, p: P, square: bool) {
    match &mut a.shape {
        Shape::Arrow(s, b) | Shape::Line(s, b) => {
            *b = if square { constrain(*s, p, true) } else { p }
        }
        Shape::Rect(s, b) | Shape::Ellipse(s, b) | Shape::Mosaic(s, b) => {
            *b = if square { constrain(*s, p, false) } else { p }
        }
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

/// 編集モードの押下: 文字の入力中なら確定する。テキストの道具なら、続けて押した所で新しく入力を始める
/// (ほかの道具 = 選択の道具での再編集は、確定だけ)。選択の道具は、注釈に当たれば選んで動かし、外れたら矩形選択を
/// 始める (文字のダブルクリックは再編集)。描く道具は注釈に当てず、いつも描き始める。
pub fn down(host: &HostRef) {
    if finish_text(host, true) && host.borrow().editor().is_none_or(|e| e.tool != Tool::Text) {
        return;
    }
    let c = cursor();
    let now = unsafe { GetMessageTime() } as u32;
    let start_text = {
        let mut guard = host.borrow_mut();
        let h = &mut *guard;
        let at = image_point(h, c);
        let select = h.editor().is_some_and(|e| e.tool == Tool::Select);
        let grip = handle_at(h, c);
        let hit = if select { hit(h, at) } else { None };
        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        let tag = h.tag();
        let (doc, edit) = h.parts();
        let Some(e) = edit else { return };
        let (t0, c0) = e.last_down;
        let dbl = now.wrapping_sub(t0) <= unsafe { GetDoubleClickTime() }
            && (c.x - c0.x).abs() * 2 <= unsafe { GetSystemMetrics(SM_CXDOUBLECLK) }
            && (c.y - c0.y).abs() * 2 <= unsafe { GetSystemMetrics(SM_CYDOUBLECLK) };
        e.last_down = (now, c);
        let hit_text = hit.filter(|&i| matches!(doc.items[i].shape, Shape::Text { .. }));
        if let Some((i, handle)) = grip.filter(|_| !ctrl) {
            e.drag = Some(Drag::Resize {
                i,
                handle,
                orig: doc.items[i].clone(),
                start: at,
            });
            None
        } else if dbl
            && !ctrl
            && let Some(i) = hit_text
        {
            e.sel = vec![i];
            Some((at, Some(i)))
        } else if ctrl && let Some(i) = hit {
            // Ctrl+クリック: 選択に足す・外す。足したら選んだ物をまとめて動かせる。
            match e.sel.binary_search(&i) {
                Ok(k) => {
                    e.sel.remove(k);
                }
                Err(k) => {
                    e.sel.insert(k, i);
                    e.drag = Some(Drag::Move {
                        last: at,
                        total: (0.0, 0.0),
                    });
                }
            }
            eprintln!("{tag}: ctrl select {:?}", e.sel);
            None
        } else if select && ctrl {
            // Ctrl で空きから引く矩形選択は、今の選択に足す。
            e.drag = Some(Drag::Marquee(at, at));
            None
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
    let h = host.borrow();
    h.repaint();
    // 選択が変わった (Ctrl+クリックで外したときは `up` が何もしない): 文字の装飾のボタンの可否を描き直す。
    h.toolbar_changed();
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
        Some(Drag::Draw(a)) => drag_to(a, at, shift()),
        Some(Drag::Move { last, total }) => {
            let d = (at.0 - last.0, at.1 - last.1);
            *last = at;
            *total = (total.0 + d.0, total.1 + d.1);
            for &i in &e.sel {
                doc.nudge(i, d);
            }
        }
        Some(Drag::Marquee(_, q)) => *q = at,
        Some(Drag::Resize {
            i,
            handle,
            orig,
            start,
        }) => {
            let a = resized(orig, *handle, (at.0 - start.0, at.1 - start.1), shift());
            if let Some(now) = doc.items.get_mut(*i) {
                *now = a;
            }
        }
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
            eprintln!(
                "{tag}: moved {:?} by ({:.1}, {:.1})",
                e.sel, total.0, total.1
            );
        }
        Drag::Resize { i, orig, .. } => {
            commit_resize(doc, i, orig);
            eprintln!("{tag}: resized {i}");
        }
        Drag::Marquee(p, q) => {
            e.sel = marquee_pick(&doc.items, p, q, &pickable, &e.sel);
            eprintln!("{tag}: marquee select {:?}", e.sel);
        }
    }
    drop(guard);
    let h = host.borrow();
    h.repaint();
    h.toolbar_changed();
}

/// 矩形選択で選ぶ注釈 (昇順): 矩形に触れる注釈と `sel` を合わせる (Ctrl で始めた矩形選択は押下で選択を
/// 外していない: 今の選択に足す)。引かずに離した空きのクリック (2 px 未満) は、外接矩形の内側で拾わない。
fn marquee_pick(items: &[Annot], p: P, q: P, pickable: &[bool], sel: &[usize]) -> Vec<usize> {
    if (p.0 - q.0).abs() < 2.0 && (p.1 - q.1).abs() < 2.0 {
        return sel.to_vec();
    }
    let mut v = touching(items, p, q, pickable);
    v.extend_from_slice(sel);
    v.sort_unstable();
    v.dedup();
    v
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
    widgets::register_class(TEXT_CLASS, text_proc);
    let (owner, tag, at, screen, k, s, text, ink, size, deco) = {
        let h = host.borrow();
        let l = last();
        // 再編集は元の注釈の装飾を引き継ぐ。
        let (text, ink, size, at, deco) = match replacing.and_then(|i| h.doc().items.get(i)) {
            Some(Annot {
                shape: Shape::Text { text, at, deco, .. },
                ink,
                size,
                ..
            }) => (text.clone(), *ink, *size, *at, *deco),
            _ => (String::new(), l.ink, l.size as f32, at, l.deco),
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
            deco,
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
            deco,
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
    restart_blink(text_host);
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
        let m = (!text.trim().is_empty()).then(|| ui::ink_text(&text, t.size));
        commit_text(doc, &t, &text, m.map(|m| (m.w, m.h)));
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

/// 文字の確定を `doc` に当てる。`ink` は測った外形 (空の文字なら None: 再編集なら消す)。
/// 再編集は元の注釈の線幅を引き継ぐ: 文字を変えなければ `replace` は何も積まない (redo の列も残る)。
fn commit_text(doc: &mut Doc, t: &TextEdit, text: &str, ink: Option<(f32, f32)>) {
    let Some((w, h)) = ink else {
        if let Some(i) = t.replacing {
            doc.remove(&[i]);
        }
        return;
    };
    let width = t
        .replacing
        .and_then(|i| doc.items.get(i))
        .map_or(last().width as f32, |a| a.width);
    let a = Annot {
        shape: Shape::Text {
            at: t.at,
            text: text.to_string(),
            w,
            h,
            deco: t.deco,
        },
        ink: t.ink,
        width,
        size: t.size,
        fill: false,
    };
    match t.replacing {
        Some(i) => doc.replace(i, a),
        None => {
            doc.add(a);
        }
    }
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
        // Ctrl+A: 全部選ぶ (1 行の EDIT は持たない)。
        WM_KEYDOWN if wparam.0 == 'A' as usize && crate::pin::ctrl() => unsafe {
            SendMessageW(hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
            restart_blink(text_host);
            repaint(text_host);
            LRESULT(0)
        },
        // Ctrl+Backspace: 左の 1 語を消す (EDIT はそのままだと □ を打つ)。選んでいれば選んだ所を消す。
        WM_KEYDOWN if wparam.0 as u16 == VK_BACK.0 && crate::pin::ctrl() => unsafe {
            let (_, (a, b)) = edit_state(hwnd);
            if a == b {
                let n = GetWindowTextLengthW(hwnd) as usize;
                let mut buf = vec![0u16; n + 1];
                let n = GetWindowTextW(hwnd, &mut buf) as usize;
                let from = word_left(&buf[..n], b as usize);
                SendMessageW(
                    hwnd,
                    EM_SETSEL,
                    Some(WPARAM(from)),
                    Some(LPARAM(b as isize)),
                );
            }
            SendMessageW(
                hwnd,
                EM_REPLACESEL,
                Some(WPARAM(1)),
                Some(LPARAM(w!("").0 as isize)),
            );
            restart_blink(text_host);
            repaint(text_host);
            LRESULT(0)
        },
        // 1 行の EDIT は Enter と Esc で鳴る。Ctrl+A (0x01) も鳴り、Ctrl+Backspace (0x7F) は □ を打つ。
        WM_CHAR if matches!(wparam.0, 0x0D | 0x1B | 0x01 | 0x7F) => LRESULT(0),
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
            restart_blink(text_host);
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
                    restart_blink(hwnd);
                    repaint(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == BLINK_TIMER => {
            CARET_ON.with(|c| c.set(!c.get()));
            repaint(hwnd);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// EDIT の文字と選択 (始め, 終わり。UTF-16)。何も選んでいなければ両方カーソルの位置。
fn edit_state(edit: HWND) -> (String, (u32, u32)) {
    unsafe {
        let n = GetWindowTextLengthW(edit) as usize;
        let mut buf = vec![0u16; n + 1];
        let n = GetWindowTextW(edit, &mut buf) as usize;
        // 下位が選択の始め、上位が終わり。
        let sel = SendMessageW(edit, EM_GETSEL, None, None).0 as u32;
        (
            String::from_utf16_lossy(&buf[..n]),
            (sel & 0xFFFF, sel >> 16),
        )
    }
}

/// Ctrl+Backspace で消す範囲の始め: カーソルの左の空白を飛ばし、その左の 1 語の頭。
fn word_left(text: &[u16], caret: usize) -> usize {
    let space = |c: u16| char::from_u32(c as u32).is_some_and(char::is_whitespace);
    let mut i = caret.min(text.len());
    while i > 0 && space(text[i - 1]) {
        i -= 1;
    }
    while i > 0 && !space(text[i - 1]) {
        i -= 1;
    }
    i
}

/// カーソルを出し直して点滅の間を数え直す (打っている間は消えない)。点滅しない設定なら張らない。
fn restart_blink(text_host: HWND) {
    CARET_ON.with(|c| c.set(true));
    let ms = unsafe { GetCaretBlinkTime() };
    if ms != 0 && ms != u32::MAX {
        unsafe {
            SetTimer(Some(text_host), BLINK_TIMER, ms, None);
        }
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

/// 注釈のツールバーのポップアップ。どれも選んでも閉じない (見比べて選ぶ)。外をクリックすると閉じる。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    Color,
    Width,
    Size,
    Deco,
}

/// 注釈のツールバーのボタンを押した。
pub enum Action {
    Tool(Tool),
    Popup(Choice),
    /// 四角・丸の塗りを入れ替える (`toggle_fill`)。
    Fill,
    Undo,
    Redo,
    Done,
}

/// 図形 / 文字 / 塗り / 色、線幅、文字の装飾 / 元に戻す、やり直す / 完了。完了は `check` のツールのボタン (反転した面なし)。
pub fn items() -> Vec<Item> {
    let mut items = vec![Item::Sep; A_DONE + 1];
    for (i, _, icon) in A_TOOLS {
        items[i] = Item::Tool(icon);
    }
    items[A_COLOR] = Item::Slot;
    items[A_WIDTH] = Item::Slot;
    items[A_DECO] = Item::Slot;
    items[A_UNDO] = Item::Tool("undo");
    items[A_REDO] = Item::Tool("redo");
    items[A_DONE] = Item::Tool("check");
    items
}

fn tool_at(i: usize) -> Option<Tool> {
    A_TOOLS.iter().find(|t| t.0 == i).map(|t| t.1)
}

/// 線幅のボタンの役と色の見本を決める道具: 選択の道具でテキストだけを選んでいればテキストの道具
/// (線幅のボタンが文字の大きさになる。文字は線幅で見た目が変わらない)。ほかは今の道具。
pub fn slot_tool(h: &dyn Host) -> Tool {
    let Some(e) = h.editor() else {
        return Tool::Arrow;
    };
    let all = |f: fn(&Shape) -> bool| {
        !e.sel.is_empty()
            && e.sel
                .iter()
                .all(|&i| h.doc().items.get(i).is_some_and(|a| f(&a.shape)))
    };
    match e.tool {
        Tool::Select if all(|s| matches!(s, Shape::Text { .. })) => Tool::Text,
        // 四角・丸だけを選んでいれば、装飾のボタンが塗りのボタンになる (丸だけなら丸の印)。
        Tool::Select if all(|s| matches!(s, Shape::Ellipse(..))) => Tool::Ellipse,
        Tool::Select if all(|s| matches!(s, Shape::Rect(..) | Shape::Ellipse(..))) => Tool::Rect,
        t => t,
    }
}

fn fillable(a: &Annot) -> bool {
    matches!(a.shape, Shape::Rect(..) | Shape::Ellipse(..))
}

/// 色・線幅 (文字の大きさ)・文字の装飾のボタンとポップアップに出す値。値の `None` は、選んだ注釈で値が混ざっている。
#[derive(Clone, Copy)]
pub struct Shown {
    /// 線幅のボタンの役と色の見本を決める道具 (`slot_tool`)。
    pub tool: Tool,
    /// 色と線幅のボタンを押せるか。選択の道具で何も選んでいなければ押せない。
    pub style: bool,
    /// 文字の装飾のボタンを押せるか (`deco_on`)。
    pub deco_on: bool,
    pub ink: Option<usize>,
    pub width: Option<u32>,
    pub size: Option<u32>,
    pub deco: Option<Deco>,
    /// 四角・丸を塗るか。装飾のボタンは `tool` が四角・丸なら塗りのトグル (`fill_on`)。
    pub fill: Option<bool>,
}

impl Shown {
    /// 装飾のボタンが塗りのトグルになっている。
    pub fn fill_on(&self) -> bool {
        matches!(self.tool, Tool::Rect | Tool::Ellipse)
    }
}

/// 今の表示の値: 選択の道具で選んでいれば選んだ注釈の値、ほかは次に描く値 (`last`)。
/// 線幅はテキスト以外、文字の大きさと装飾はテキスト、色はモザイク以外から取る。取る注釈が無ければ `last`。
pub fn shown(h: &dyn Host) -> Shown {
    let l = last();
    let tool = slot_tool(h);
    let mut sh = Shown {
        tool,
        style: true,
        deco_on: deco_on(h),
        ink: Some(l.ink_for(tool)),
        width: Some(l.width),
        size: Some(l.size),
        deco: Some(l.deco),
        fill: Some(l.fill),
    };
    let Some(e) = h.editor().filter(|e| e.tool == Tool::Select) else {
        return sh;
    };
    sh.style = !e.sel.is_empty();
    let items: Vec<&Annot> = e.sel.iter().filter_map(|&i| h.doc().items.get(i)).collect();
    let text = |a: &&&Annot| matches!(a.shape, Shape::Text { .. });
    let paint = || items.iter().filter(|a| !a.is_mosaic());
    common(&mut sh.ink, paint().map(|a| a.ink));
    // モザイクは線幅で升の粗さが決まる。
    common(
        &mut sh.width,
        items
            .iter()
            .filter(|a| !text(a))
            .map(|a| a.width.round() as u32),
    );
    common(
        &mut sh.size,
        items.iter().filter(text).map(|a| a.size.round() as u32),
    );
    common(
        &mut sh.deco,
        items.iter().filter_map(|a| match a.shape {
            Shape::Text { deco, .. } => Some(deco),
            _ => None,
        }),
    );
    common(
        &mut sh.fill,
        items.iter().filter(|a| fillable(a)).map(|a| a.fill),
    );
    sh
}

/// 塗りのボタンを押した: 塗る・塗らないを入れ替えて覚え、選んでいる四角・丸にも当てる (混ざっていれば塗る)。
pub fn toggle_fill(host: &RefCell<dyn Host>) {
    let on = !shown(&*host.borrow()).fill.unwrap_or(false);
    LAST.with(|l| {
        l.set(Last {
            fill: on,
            ..l.get()
        })
    });
    let mut guard = host.borrow_mut();
    let (doc, edit) = guard.parts();
    let sel = edit.map(|e| e.sel.clone()).unwrap_or_default();
    let news = sel
        .into_iter()
        .filter_map(|i| doc.items.get(i).map(|a| (i, a)))
        .filter(|(_, a)| fillable(a) && a.fill != on)
        .map(|(i, a)| {
            (
                i,
                Annot {
                    fill: on,
                    ..a.clone()
                },
            )
        })
        .collect();
    doc.replace_all(news);
}

/// `it` が空なら `v` のまま。全部同じならその値、混ざっていれば `None`。
fn common<T: PartialEq>(v: &mut Option<T>, mut it: impl Iterator<Item = T>) {
    if let Some(first) = it.next() {
        let same = it.all(|x| x == first);
        *v = same.then_some(first);
    }
}

/// 文字の装飾のボタンを押せるか: 文字の道具のとき、または選択の道具でテキストを選んでいるとき。
pub fn deco_on(h: &dyn Host) -> bool {
    h.editor().is_some_and(|e| match e.tool {
        Tool::Text => true,
        Tool::Select => e.sel.iter().any(|&i| {
            h.doc()
                .items
                .get(i)
                .is_some_and(|a| matches!(a.shape, Shape::Text { .. }))
        }),
        _ => false,
    })
}

/// ボタン `i` の働き (線幅のボタンは `sh.tool` が文字の道具なら文字の大きさになる)。押せないボタンは None。
pub fn action(i: usize, sh: &Shown) -> Option<Action> {
    if let Some(t) = tool_at(i) {
        return Some(Action::Tool(t));
    }
    match i {
        A_COLOR if sh.style => Some(Action::Popup(Choice::Color)),
        A_WIDTH if sh.style && sh.tool == Tool::Text => Some(Action::Popup(Choice::Size)),
        A_WIDTH if sh.style => Some(Action::Popup(Choice::Width)),
        A_DECO if sh.fill_on() => Some(Action::Fill),
        A_DECO if sh.deco_on => Some(Action::Popup(Choice::Deco)),
        A_UNDO => Some(Action::Undo),
        A_REDO => Some(Action::Redo),
        A_DONE => Some(Action::Done),
        _ => None,
    }
}

/// ポップアップ `c` が今もボタンの役のままか (押せないボタン・役が替わったボタンのポップアップは閉じる)。
pub fn popup_valid(c: Choice, sh: &Shown) -> bool {
    let (i, ..) = popup_geometry(c, 1.0);
    matches!(action(i, sh), Some(Action::Popup(k)) if k == c)
}

/// ボタン `i` の (オン, 無効, ポップアップが開いている)。`tool` は今の道具。
pub fn button(
    i: usize,
    tool: Tool,
    undo: bool,
    redo: bool,
    sh: &Shown,
    open: Option<Choice>,
) -> (bool, bool, bool) {
    (
        tool_at(i) == Some(tool),
        (i == A_UNDO && !undo)
            || (i == A_REDO && !redo)
            || (matches!(i, A_COLOR | A_WIDTH) && !sh.style)
            || (i == A_DECO && !sh.deco_on && !sh.fill_on()),
        (i == A_COLOR && open == Some(Choice::Color))
            || (i == A_WIDTH && matches!(open, Some(Choice::Width | Choice::Size)))
            || (i == A_DECO && open == Some(Choice::Deco)),
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
        A_DECO if matches!(tool, Tool::Rect | Tool::Ellipse) => (t.fill, ""),
        A_DECO => (t.text_deco, ""),
        A_UNDO => (t.undo, "Ctrl+Z"),
        A_REDO => (t.redo, "Ctrl+Y"),
        A_DONE => (t.done, ""),
        _ => ("", ""),
    }
}

fn ink_color(i: usize) -> D2D1_COLOR_F {
    ui::hex(annot::INKS[i])
}

/// 色・線幅 (文字の道具では文字の大きさ)・文字の装飾の枠の中身。`layout` は `toolbar_layout` の結果。
/// 混ざった値は「–」(色は地なしの四角に「–」)。押せないボタンは `text-disabled` で、色の見本は薄くする。
#[allow(clippy::too_many_arguments)]
pub fn draw_slots(
    ctx: &ID2D1DeviceContext,
    layout: &[(i32, i32)],
    sh: &Shown,
    color: BtnState,
    width: BtnState,
    deco: BtnState,
    pal: &Palette,
    s: f32,
) {
    if sh.fill_on() {
        draw_fill_slot(ctx, layout[A_DECO], sh, deco, pal, s);
    } else {
        draw_deco_slot(ctx, layout[A_DECO], sh.deco, deco, pal, s);
    }
    let top = px(4.0, s) as f32;
    let fg = |st: BtnState| {
        if st == BtnState::Disabled {
            pal.text_disabled
        } else {
            pal.text_primary
        }
    };
    let (cx, cb) = layout[A_COLOR];
    let ink = match sh.ink {
        Some(i) if color == BtnState::Disabled => D2D1_COLOR_F {
            a: 0.35,
            ..ink_color(i)
        },
        Some(i) => ink_color(i),
        None => D2D1_COLOR_F::default(),
    };
    widgets::swatch(ctx, cx as f32, top, ink, color, false, pal, s);
    if sh.ink.is_none() {
        mixed_mark(
            ctx,
            cx as f32 + cb as f32 / 2.0,
            top + cb as f32 / 2.0,
            fg(color),
            s,
        );
    }
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
    let ink = fg(width);
    match (sh.tool == Tool::Text, sh.size, sh.width) {
        (true, None, _) | (false, _, None) => mixed_mark(ctx, wx + b / 2.0, top + b / 2.0, ink, s),
        (true, Some(size), _) => {
            let t = ui::text(&size.to_string(), Style::NumStrong, s);
            ui::draw_text(
                ctx,
                &t,
                (wx + (b - t.w) / 2.0).round(),
                (top + (b - t.h) / 2.0).round(),
                ink,
            );
        }
        (false, _, Some(lw)) => {
            // 線幅の印: 今の線幅の数字と、その下にその太さの線 (間 2、太さは 6 で頭打ち)。
            // 横線 1 本だけだと最小化の印に見える。
            let t = ui::text(&lw.to_string(), Style::NumStrong, s);
            let w = (lw as f32).min(6.0);
            let (gap, bar) = (px(2.0, s) as f32, px(w, s) as f32);
            let y = (top + (b - t.h - gap - bar) / 2.0).round();
            ui::draw_text(ctx, &t, (wx + (b - t.w) / 2.0).round(), y, ink);
            widgets::width_sample(ctx, wx + b / 2.0, y + t.h + gap + bar / 2.0, w, ink, s);
        }
    }
}

/// 値が混ざっている印: (cx, cy) を中心に 2 × 2 の点を 3 つ、間 2 で横に並べる。
/// 横線 1 本だと最小化の印に見える (線幅のボタンの印と同じ理由)。
fn mixed_mark(ctx: &ID2D1DeviceContext, cx: f32, cy: f32, c: D2D1_COLOR_F, s: f32) {
    let d = px(2.0, s) as f32;
    let (x0, y) = ((cx - 2.5 * d).round(), (cy - d / 2.0).round());
    for k in 0..3 {
        let x = x0 + k as f32 * 2.0 * d;
        ui::fill_rect(ctx, rectf(x, y, x + d, y + d), c);
    }
}

/// 文字の装飾のボタン: 今の装飾の見本の「A」。なしは 16 px の文字。縁取りは 16 px の文字を `on-content` で
/// 塗り、輪郭にボタンの文字色の線 3 (外へ 1.5、丸い角)。背景は 18 × 18・角丸 3 の箱をボタンの文字色で塗り、
/// 13 px の文字を `on-content` で抜く (DS `AnnotationToolbar`)。混ざっていれば横線 (`mixed_mark`)。
fn draw_deco_slot(
    ctx: &ID2D1DeviceContext,
    (x, bw): (i32, i32),
    deco: Option<Deco>,
    state: BtnState,
    pal: &Palette,
    s: f32,
) {
    let top = px(4.0, s) as f32;
    let (x, b) = (x as f32, bw as f32);
    let back = match state {
        BtnState::Hover => Some(pal.fill_hover),
        BtnState::Pressed | BtnState::Open => Some(pal.fill_pressed),
        _ => None,
    };
    if let Some(c) = back {
        ui::fill_round(ctx, rectf(x, top, x + b, top + b), px(4.0, s) as f32, c);
    }
    let fg = if state == BtnState::Disabled {
        pal.text_disabled
    } else {
        pal.text_primary
    };
    let (cx, cy) = (x + b / 2.0, top + b / 2.0);
    let Some(deco) = deco else {
        return mixed_mark(ctx, cx, cy, fg, s);
    };
    let size = if deco == Deco::Box { 13.0 } else { 16.0 };
    let t = ui::ink_text("A", px(size, s) as f32);
    let (tx, ty) = ((cx - t.w / 2.0).round(), (cy - t.h / 2.0).round());
    match deco {
        Deco::None => ui::draw_text(ctx, &t, tx, ty, fg),
        Deco::Outline => unsafe {
            let (Some(g), Ok(rim), Ok(fill)) = (
                ui::text_outline(&t, tx, ty),
                ctx.CreateSolidColorBrush(&fg, None),
                ctx.CreateSolidColorBrush(&pal.on_content, None),
            ) else {
                return;
            };
            ctx.DrawGeometry(&g, &rim, px(3.0, s) as f32, &widgets::round_stroke());
            ctx.FillGeometry(&g, &fill, None);
        },
        Deco::Box => {
            let h = px(18.0, s) as f32 / 2.0;
            ui::fill_round(
                ctx,
                rectf(cx - h, cy - h, cx + h, cy + h),
                px(3.0, s) as f32,
                fg,
            );
            ui::draw_text(ctx, &t, tx, ty, pal.on_content);
        }
    }
}

/// 塗りのボタン: 16 × 16 の四角 (丸の道具では丸)。塗らないは線 1.5 の輪郭、塗るは中までボタンの文字色。
/// 混ざっていれば `mixed_mark`。
fn draw_fill_slot(
    ctx: &ID2D1DeviceContext,
    (x, bw): (i32, i32),
    sh: &Shown,
    state: BtnState,
    pal: &Palette,
    s: f32,
) {
    let top = px(4.0, s) as f32;
    let (x, b) = (x as f32, bw as f32);
    let back = match state {
        BtnState::Hover => Some(pal.fill_hover),
        BtnState::Pressed | BtnState::Open => Some(pal.fill_pressed),
        _ => None,
    };
    if let Some(c) = back {
        ui::fill_round(ctx, rectf(x, top, x + b, top + b), px(4.0, s) as f32, c);
    }
    let fg = if state == BtnState::Disabled {
        pal.text_disabled
    } else {
        pal.text_primary
    };
    let (cx, cy) = (x + b / 2.0, top + b / 2.0);
    let Some(on) = sh.fill else {
        return mixed_mark(ctx, cx, cy, fg, s);
    };
    let (h, lw) = (px(16.0, s) as f32 / 2.0, px(1.5, s) as f32);
    // 線は内側に収める (塗りと同じ外形)。
    let r = rectf(
        cx - h + lw / 2.0,
        cy - h + lw / 2.0,
        cx + h - lw / 2.0,
        cy + h - lw / 2.0,
    );
    unsafe {
        let Ok(brush) = ctx.CreateSolidColorBrush(&fg, None) else {
            return;
        };
        if sh.tool == Tool::Ellipse {
            let e = D2D1_ELLIPSE {
                point: windows_numerics::Vector2 { X: cx, Y: cy },
                radiusX: h - lw / 2.0,
                radiusY: h - lw / 2.0,
            };
            if on {
                ctx.FillEllipse(&e, &brush);
            }
            ctx.DrawEllipse(&e, &brush, lw, None);
        } else {
            let rr = D2D1_ROUNDED_RECT {
                rect: r,
                radiusX: px(2.0, s) as f32,
                radiusY: px(2.0, s) as f32,
            };
            if on {
                ctx.FillRoundedRectangle(&rr, &brush);
            }
            ctx.DrawRoundedRectangle(&rr, &brush, lw, None);
        }
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
        // 文字の大きさと同じ選択肢の並び (幅 212)。
        Choice::Deco => (
            A_DECO,
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
        Choice::Deco => widgets::seg_hit(annot::DECOS.len(), widgets::SIZE_POPUP_W, x, y, s),
    }
}

/// 色・線幅・文字の大きさのポップアップの中身。`pw` はポップアップの幅。
pub fn draw_popup(
    ctx: &ID2D1DeviceContext,
    kind: Choice,
    sh: &Shown,
    hover: Option<usize>,
    pw: f32,
    pal: &Palette,
    s: f32,
) {
    let strings = crate::strings();
    // 混ざっていれば、見出しの値は「混在」で、どの選択肢にも印を付けない (選ぶと揃う)。
    let px_label = |v: Option<u32>| v.map_or(strings.mixed.to_string(), |v| format!("{v} px"));
    match kind {
        Choice::Color => {
            let sel = sh.ink;
            let head = sel.map_or(strings.mixed, |i| strings.inks[i]);
            widgets::popup_head(ctx, strings.color, head, pw, pal, s);
            let colors: Vec<_> = (0..annot::INKS.len()).map(ink_color).collect();
            widgets::draw_color_choices(ctx, &colors, sel, hover, pal, s);
        }
        Choice::Width => {
            let sel = sh
                .width
                .and_then(|v| annot::WIDTHS.iter().position(|&w| w == v));
            let head = px_label(sh.width);
            widgets::popup_head(ctx, strings.line_width, &head, pw, pal, s);
            widgets::draw_width_choices(ctx, &annot::WIDTHS, sel, hover, pal, s);
        }
        Choice::Size => {
            let sel = sh
                .size
                .and_then(|v| annot::SIZES.iter().position(|&w| w == v));
            let head = px_label(sh.size);
            widgets::popup_head(ctx, strings.text_size, &head, pw, pal, s);
            let labels: Vec<String> = annot::SIZES.iter().map(|v| v.to_string()).collect();
            widgets::draw_seg(ctx, &labels, sel, hover, widgets::SIZE_POPUP_W, pal, s);
        }
        Choice::Deco => {
            let sel = sh
                .deco
                .and_then(|v| annot::DECOS.iter().position(|&d| d == v));
            let head = sel.map_or(strings.mixed, |i| strings.decos[i]);
            widgets::popup_head(ctx, strings.text_deco, head, pw, pal, s);
            let labels: Vec<String> = strings.decos.iter().map(|v| v.to_string()).collect();
            widgets::draw_seg(ctx, &labels, sel, hover, widgets::SIZE_POPUP_W, pal, s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表示の値: 取る注釈が無ければ元の値、全部同じならその値、混ざれば None (「混在」)。
    #[test]
    fn common_value_or_mixed() {
        let run = |v: &[u32]| {
            let mut x = Some(9);
            common(&mut x, v.iter().copied());
            x
        };
        assert_eq!(run(&[]), Some(9));
        assert_eq!(run(&[4, 4]), Some(4));
        assert_eq!(run(&[4, 2, 4]), None);
    }

    /// 貼り付けは 1 手 (1 回の undo で全部消える)、続けて貼ると階段状、バッジは振り直す。
    #[test]
    fn paste_steps_and_renumbers() {
        let mut doc = Doc::default();
        let badge = Annot {
            shape: Shape::Badge {
                at: (0.0, 0.0),
                n: doc.take_badge(),
            },
            ..rect_at(0.0)
        };
        doc.add(badge.clone());
        let mut clip = vec![rect_at(0.0), badge];
        assert_eq!(pasted(&mut clip, (5.0, 5.0), &mut doc), 1..3);
        assert_eq!(pasted(&mut clip, (5.0, 5.0), &mut doc), 3..5);
        assert_eq!(doc.items[3].shape, Shape::Rect((10.0, 20.0), (50.0, 40.0)));
        assert!(matches!(doc.items[4].shape, Shape::Badge { n: 3, .. }));
        assert!(doc.undo());
        assert_eq!(doc.items.len(), 3);
    }

    /// Ctrl+Backspace: カーソルの左の空白を飛ばして 1 語。語の途中なら語の頭まで。
    #[test]
    fn ctrl_backspace_word() {
        let w: Vec<u16> = "foo bar  baz".encode_utf16().collect();
        assert_eq!(word_left(&w, 12), 9); // 末尾 → "baz" の頭
        assert_eq!(word_left(&w, 9), 4); // 空白 2 つを飛ばして "bar" の頭
        assert_eq!(word_left(&w, 6), 4); // "bar" の途中
        assert_eq!(word_left(&w, 2), 0);
        assert_eq!(word_left(&w, 0), 0);
    }

    /// カーソルへの貼り付け: 覚えた物全体の外接矩形の中心がカーソルに来る。
    #[test]
    fn paste_centers_on_cursor() {
        // 2 つの箱は x 0〜140、y 10〜30 (線幅の分は上下左右に同じだけ広がるので中心は動かない)。中心 (70, 20)。
        let clip = [rect_at(0.0), rect_at(100.0)];
        assert_eq!(to_cursor(&clip, (500.0, 300.0)), (430.0, 280.0));
    }

    fn rect_at(x: f32) -> Annot {
        Annot {
            shape: Shape::Rect((x, 10.0), (x + 40.0, 30.0)),
            ink: annot::INK_RED,
            width: 4.0,
            size: 18.0,
            fill: false,
        }
    }

    /// Shift: 線は水平・垂直・45°、箱は正方形。描くときも角のハンドルで引くときも同じ。どの向きに引いても符号は保つ。
    #[test]
    fn shift_constrains_draw_and_resize() {
        let annot = |shape| Annot {
            shape,
            ..rect_at(0.0)
        };
        let o = (100.0, 100.0);
        for (to, line, square) in [
            ((130.0, 110.0), (130.0, 100.0), (130.0, 130.0)),
            ((90.0, 140.0), (100.0, 140.0), (60.0, 140.0)),
            ((60.0, 80.0), (60.0, 60.0), (60.0, 60.0)),
            ((105.0, 50.0), (100.0, 50.0), (150.0, 50.0)),
        ] {
            let mut a = annot(Shape::Line(o, o));
            drag_to(&mut a, to, true);
            assert!(
                matches!(a.shape, Shape::Line(_, b) if b == line),
                "line {to:?}"
            );
            let mut a = annot(Shape::Ellipse(o, o));
            drag_to(&mut a, to, true);
            assert!(
                matches!(a.shape, Shape::Ellipse(_, b) if b == square),
                "box {to:?}"
            );
        }
        // 線の端を引く: もう一方の端から見て水平に。
        let l = resized(&annot(Shape::Line(o, (150.0, 100.0))), 1, (5.0, 20.0), true);
        assert!(matches!(l.shape, Shape::Line(p, q) if p == o && q == (155.0, 100.0)));
        // 40x20 の右下の角を (+10, +5) 引く: 左上を止めて 50x50 の正方形。
        let r = resized(&rect_at(10.0), 7, (10.0, 5.0), true);
        assert_eq!(r.core_box(), Some((10.0, 10.0, 60.0, 60.0)));
        // 辺のハンドルと Shift なしは揃えない。
        let r = resized(&rect_at(10.0), 6, (0.0, 5.0), true);
        assert_eq!(r.core_box(), Some((10.0, 10.0, 50.0, 35.0)));
    }

    /// 移動のドラッグ中に選択を外す (Esc) か消す (Delete): 移動は記録され、undo で元の位置へ戻る。
    #[test]
    fn drop_sel_mid_move_records_the_move() {
        for remove in [false, true] {
            let mut doc = Doc::default();
            doc.add(rect_at(10.0));
            let mut e = Editor::new();
            e.sel = vec![0];
            e.drag = Some(Drag::Move {
                last: (0.0, 0.0),
                total: (25.0, 5.0),
            });
            doc.nudge(0, (25.0, 5.0));
            let (sel, dragged) = take_sel(&mut doc, &mut e);
            assert_eq!((sel.clone(), dragged), (vec![0], true));
            assert!(e.drag.is_none() && e.sel.is_empty());
            if remove {
                doc.remove(&sel);
                assert!(doc.undo(), "remove={remove}: undo remove");
            }
            assert!(doc.undo(), "remove={remove}: undo move");
            assert_eq!(doc.items, vec![rect_at(10.0)], "remove={remove}");
        }
    }

    /// 文字を変えない再編集の確定は何も積まない。線幅は今の既定 (4) と、装飾は既定 (なし) と違う注釈で試す。
    #[test]
    fn unchanged_text_reedit_records_nothing() {
        let mut doc = Doc::default();
        let orig = Annot {
            shape: Shape::Text {
                at: (5.0, 6.0),
                text: "abc".into(),
                w: 30.0,
                h: 20.0,
                deco: Deco::Box,
            },
            ink: annot::INK_RED,
            width: 8.0,
            size: 24.0,
            fill: false,
        };
        assert_ne!(orig.width, last().width as f32);
        doc.add(orig.clone());
        doc.add(rect_at(100.0));
        assert!(doc.undo());
        let depth = doc.depth();
        let t = TextEdit {
            host: HWND::default(),
            edit: HWND::default(),
            at: (5.0, 6.0),
            ink: annot::INK_RED,
            size: 24.0,
            deco: Deco::Box,
            replacing: Some(0),
            font: HFONT::default(),
            pad: 0,
            font_px: 0,
            composing: false,
        };
        commit_text(&mut doc, &t, "abc", Some((30.0, 20.0)));
        assert_eq!(doc.depth(), depth);
        assert_eq!(doc.items, vec![orig]);
        assert!(doc.can_redo(), "redo の列が残る");
    }
}
