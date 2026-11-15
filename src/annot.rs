//! ピンの注釈: オブジェクト、コマンド列の undo/redo、ヒットテスト、モザイク、描画と合成。
//! GIF ピンの注釈の区間・undo の印の列・書き出しの層もここ (区間は `Annot` の外、GIF ピン側で持つ)。
//! 座標と寸法は画像の物理 px。規則は DS「Odile」の `AnnotationToolbar`。

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::core::Result;
use windows_numerics::Vector2;

use std::cell::RefCell;
use std::ops::Range;

use crate::capture::Image;
use crate::timeline::Timeline;
use crate::ui;

pub type P = (f32, f32);
/// 整数の矩形 (左, 上, 右, 下)。
pub type Ltrb = (u32, u32, u32, u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    /// 選択: クリックで最上部の 1 個、ドラッグで矩形に触れる物を選ぶ。描かない。
    Select,
    Arrow,
    Line,
    Rect,
    Ellipse,
    Text,
    Badge,
    Pen,
    Highlighter,
    Mosaic,
}

/// `ink-*` の 7 色 (red, orange, yellow, green, blue, black, white)。ユーザーが描く色なので、テーマに依らない。
pub const INKS: [u32; 7] = [
    0xE8382F, 0xF28A00, 0xF5CF1F, 0x22863F, 0x1F7AE0, 0x1A1A1A, 0xFFFFFF,
];
pub const INK_RED: usize = 0;
pub const INK_YELLOW: usize = 2;
pub const INK_BLACK: usize = 5;
pub const INK_WHITE: usize = 6;
/// `annot-ghost`: GIF ピンの区間外の注釈の灰色。`INKS` の外の添字 (色の一覧には出さない)。
pub const INK_GHOST: usize = INKS.len();
const GHOST: u32 = 0x9A9AA0;
pub const WIDTHS: [u32; 5] = [2, 4, 6, 8, 12];
pub const SIZES: [u32; 4] = [14, 18, 24, 32];
/// `opacity-highlighter`。
const HIGHLIGHTER_OPACITY: f32 = 0.4;
/// モザイクの 1 升の辺 (画像の物理 px): 線幅 × 2 (線幅 4 で 8)。
fn mosaic_cell(width: f32) -> u32 {
    (width.round() as u32 * 2).max(2)
}

/// 明るい色 (orange・yellow・white)。番号は `ink-on-light`、EDIT の地は `ink-black`。
pub fn light_ink(ink: usize) -> bool {
    matches!(ink, 1 | 2 | 6 | INK_GHOST)
}

/// テキストの装飾。縁取りと背景の色は文字色から白か黒 (`deco_ink`)。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Deco {
    #[default]
    None,
    Outline,
    Box,
}

/// 文字の装飾のポップアップの並び (なし・縁取り・背景)。
pub const DECOS: [Deco; 3] = [Deco::None, Deco::Outline, Deco::Box];

/// sRGB の相対輝度。
fn luminance(rgb: u32) -> f32 {
    let ch = |shift: u32| {
        let c = ((rgb >> shift) & 0xFF) as f32 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * ch(16) + 0.7152 * ch(8) + 0.0722 * ch(0)
}

/// 縁取りと背景の色: 文字色の相対輝度が 0.3 を超えれば黒、ほかは白 (入力中の EDIT の地と同じ分け方)。
pub fn deco_ink(ink: usize) -> usize {
    let rgb = if ink == INK_GHOST { GHOST } else { INKS[ink] };
    if luminance(rgb) > 0.3 {
        INK_BLACK
    } else {
        INK_WHITE
    }
}

/// 縁取りの太さ (字形の輪郭から外へ、画像の px): max(2, round(size ÷ 8))。
fn outline_w(size: f32) -> f32 {
    (size / 8.0).round().max(2.0)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    Arrow(P, P),
    Line(P, P),
    /// 対角の 2 点 (向きは問わない)。
    Rect(P, P),
    Ellipse(P, P),
    Mosaic(P, P),
    Pen(Vec<P>),
    Highlighter(Vec<P>),
    /// `at` は文字の箱の左上。`w`・`h` は確定したときに測った箱。`deco` は縁取り・背景。
    Text {
        at: P,
        text: String,
        w: f32,
        h: f32,
        deco: Deco,
    },
    /// `at` は中心。
    Badge {
        at: P,
        n: u32,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Annot {
    pub shape: Shape,
    pub ink: usize,
    /// 線幅 (2・4・6・8・12)。
    pub width: f32,
    /// 文字の大きさ (14・18・24・32)。テキストだけが使う。
    pub size: f32,
    /// 中を線の色で塗る。四角と丸だけが使う。
    pub fill: bool,
}

fn norm(a: P, b: P) -> (f32, f32, f32, f32) {
    (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
}

fn grow(r: (f32, f32, f32, f32), d: f32) -> (f32, f32, f32, f32) {
    (r.0 - d, r.1 - d, r.2 + d, r.3 + d)
}

fn inside(r: (f32, f32, f32, f32), p: P) -> bool {
    p.0 >= r.0 && p.0 <= r.2 && p.1 >= r.1 && p.1 <= r.3
}

fn seg_dist(p: P, a: P, b: P) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    let (x, y) = (a.0 + t * dx - p.0, a.1 + t * dy - p.1);
    (x * x + y * y).sqrt()
}

fn poly_dist(p: P, pts: &[P]) -> f32 {
    match pts {
        [] => f32::INFINITY,
        [a] => seg_dist(p, *a, *a),
        _ => pts
            .windows(2)
            .map(|w| seg_dist(p, w[0], w[1]))
            .fold(f32::INFINITY, f32::min),
    }
}

/// 矢印の頭の 2 本の端: 長さ 線幅 × 4 + 6、片側 26°。
pub fn arrow_head(a: P, b: P, w: f32) -> (P, P) {
    let len = w * 4.0 + 6.0;
    let (dx, dy) = (a.0 - b.0, a.1 - b.1);
    let d = (dx * dx + dy * dy).sqrt().max(1e-3);
    let (ux, uy) = (dx / d, dy / d);
    let (s, c) = 26f32.to_radians().sin_cos();
    (
        (b.0 + len * (ux * c - uy * s), b.1 + len * (ux * s + uy * c)),
        (
            b.0 + len * (ux * c + uy * s),
            b.1 + len * (-ux * s + uy * c),
        ),
    )
}

impl Annot {
    pub fn is_mosaic(&self) -> bool {
        matches!(self.shape, Shape::Mosaic(..))
    }

    /// 線と矢印の両端 (ハンドルは円)。
    pub fn ends(&self) -> Option<(P, P)> {
        match self.shape {
            Shape::Arrow(a, b) | Shape::Line(a, b) => Some((a, b)),
            _ => None,
        }
    }

    /// 番号バッジの直径 (縁は外に 2)。
    pub fn badge_d(&self) -> f32 {
        16.0 + self.width * 2.0
    }

    /// テキストの装飾が文字の箱の外へ出る量 (左右, 上下)。背景は round(size ÷ 4) / round(size ÷ 12)。
    pub fn deco_pad(&self) -> (f32, f32) {
        match self.shape {
            Shape::Text {
                deco: Deco::Outline,
                ..
            } => (outline_w(self.size), outline_w(self.size)),
            Shape::Text {
                deco: Deco::Box, ..
            } => ((self.size / 4.0).round(), (self.size / 12.0).round()),
            _ => (0.0, 0.0),
        }
    }

    /// 外接矩形 (線の太さとテキストの装飾を含む)。
    pub fn bbox(&self) -> (f32, f32, f32, f32) {
        let w = self.width;
        match &self.shape {
            Shape::Arrow(a, b) => {
                let (p1, p2) = arrow_head(*a, *b, w);
                let r = norm(*a, *b);
                let h = norm(p1, p2);
                grow(
                    (r.0.min(h.0), r.1.min(h.1), r.2.max(h.2), r.3.max(h.3)),
                    w / 2.0,
                )
            }
            Shape::Line(a, b) | Shape::Rect(a, b) | Shape::Ellipse(a, b) => {
                grow(norm(*a, *b), w / 2.0)
            }
            Shape::Mosaic(a, b) => norm(*a, *b),
            Shape::Pen(pts) | Shape::Highlighter(pts) => {
                let half = if matches!(self.shape, Shape::Pen(_)) {
                    w / 2.0
                } else {
                    w * 2.0
                };
                let r = pts.iter().fold(
                    (
                        f32::INFINITY,
                        f32::INFINITY,
                        f32::NEG_INFINITY,
                        f32::NEG_INFINITY,
                    ),
                    |r, p| (r.0.min(p.0), r.1.min(p.1), r.2.max(p.0), r.3.max(p.1)),
                );
                grow(r, half)
            }
            Shape::Text { at, w, h, .. } => {
                let (dx, dy) = self.deco_pad();
                (at.0 - dx, at.1 - dy, at.0 + w + dx, at.1 + h + dy)
            }
            Shape::Badge { at, .. } => {
                let r = self.badge_d() / 2.0 + 2.0;
                (at.0 - r, at.1 - r, at.0 + r, at.1 + r)
            }
        }
    }

    /// `p` (画像の px) が当たるか。線の形は線の上だけ、面の形 (モザイク・文字・バッジ) は内側。
    pub fn hit(&self, p: P, tol: f32) -> bool {
        let half = self.width / 2.0 + tol;
        match &self.shape {
            Shape::Arrow(a, b) => {
                let (p1, p2) = arrow_head(*a, *b, self.width);
                seg_dist(p, *a, *b) <= half
                    || seg_dist(p, p1, *b) <= half
                    || seg_dist(p, p2, *b) <= half
            }
            Shape::Line(a, b) => seg_dist(p, *a, *b) <= half,
            Shape::Rect(a, b) => {
                let r = norm(*a, *b);
                inside(grow(r, half), p) && (self.fill || !inside(grow(r, -half), p))
            }
            Shape::Ellipse(a, b) => {
                let r = norm(*a, *b);
                let (cx, cy) = ((r.0 + r.2) / 2.0, (r.1 + r.3) / 2.0);
                let (rx, ry) = ((r.2 - r.0) / 2.0, (r.3 - r.1) / 2.0);
                let within = |rx: f32, ry: f32| {
                    rx > 0.0
                        && ry > 0.0
                        && ((p.0 - cx) / rx).powi(2) + ((p.1 - cy) / ry).powi(2) <= 1.0
                };
                within(rx + half, ry + half) && (self.fill || !within(rx - half, ry - half))
            }
            Shape::Pen(pts) => poly_dist(p, pts) <= half,
            Shape::Highlighter(pts) => poly_dist(p, pts) <= self.width * 2.0 + tol,
            Shape::Mosaic(..) | Shape::Text { .. } => inside(grow(self.bbox(), tol), p),
            Shape::Badge { at, .. } => {
                let (dx, dy) = (p.0 - at.0, p.1 - at.1);
                (dx * dx + dy * dy).sqrt() <= self.badge_d() / 2.0 + 2.0 + tol
            }
        }
    }

    pub fn translate(&mut self, d: P) {
        let m = |p: &mut P| {
            p.0 += d.0;
            p.1 += d.1;
        };
        match &mut self.shape {
            Shape::Arrow(a, b)
            | Shape::Line(a, b)
            | Shape::Rect(a, b)
            | Shape::Ellipse(a, b)
            | Shape::Mosaic(a, b) => {
                m(a);
                m(b);
            }
            Shape::Pen(pts) | Shape::Highlighter(pts) => pts.iter_mut().for_each(m),
            Shape::Text { at, .. } | Shape::Badge { at, .. } => m(at),
        }
    }

    /// 箱のハンドルで大きさを変えられる形の芯の箱 (線の太さを含まない)。線と矢印は両端で、文字とバッジは変えない。
    pub fn core_box(&self) -> Option<(f32, f32, f32, f32)> {
        match &self.shape {
            Shape::Rect(a, b) | Shape::Ellipse(a, b) | Shape::Mosaic(a, b) => Some(norm(*a, *b)),
            Shape::Pen(pts) | Shape::Highlighter(pts) if !pts.is_empty() => Some(pts.iter().fold(
                (
                    f32::INFINITY,
                    f32::INFINITY,
                    f32::NEG_INFINITY,
                    f32::NEG_INFINITY,
                ),
                |r, p| (r.0.min(p.0), r.1.min(p.1), r.2.max(p.0), r.3.max(p.1)),
            )),
            _ => None,
        }
    }

    /// 点を箱 `from` から箱 `to` へ写す (辺を越えて引けば裏返る)。幅か高さが 0 の向きは平行移動だけ。
    pub fn fit(&mut self, from: (f32, f32, f32, f32), to: (f32, f32, f32, f32)) {
        let axis = |v: f32, a0: f32, a1: f32, b0: f32, b1: f32| {
            if (a1 - a0).abs() < 1e-3 {
                v + b0 - a0
            } else {
                b0 + (v - a0) * (b1 - b0) / (a1 - a0)
            }
        };
        let m = |p: &mut P| {
            *p = (
                axis(p.0, from.0, from.2, to.0, to.2),
                axis(p.1, from.1, from.3, to.1, to.3),
            )
        };
        match &mut self.shape {
            Shape::Arrow(a, b)
            | Shape::Line(a, b)
            | Shape::Rect(a, b)
            | Shape::Ellipse(a, b)
            | Shape::Mosaic(a, b) => {
                m(a);
                m(b);
            }
            Shape::Pen(pts) | Shape::Highlighter(pts) => pts.iter_mut().for_each(m),
            Shape::Text { at, .. } | Shape::Badge { at, .. } => m(at),
        }
    }

    /// 幅 `w` の画像の左右反転に合わせて置き直す (x → w − x)。文字とバッジは字を鏡にせず、箱ごと動かす。
    pub fn flip(&mut self, w: f32) {
        let m = |p: &mut P| p.0 = w - p.0;
        match &mut self.shape {
            Shape::Arrow(a, b)
            | Shape::Line(a, b)
            | Shape::Rect(a, b)
            | Shape::Ellipse(a, b)
            | Shape::Mosaic(a, b) => {
                m(a);
                m(b);
            }
            Shape::Pen(pts) | Shape::Highlighter(pts) => pts.iter_mut().for_each(m),
            // `at` は箱の左上: 反転後の左上は元の右上の鏡。
            Shape::Text { at, w: tw, .. } => at.0 = w - at.0 - *tw,
            Shape::Badge { at, .. } => m(at),
        }
    }

    /// 描き終えたときに捨てる (クリックだけで大きさが無い)。
    pub fn degenerate(&self) -> bool {
        let tiny = |a: P, b: P| (a.0 - b.0).abs() < 2.0 && (a.1 - b.1).abs() < 2.0;
        match &self.shape {
            Shape::Arrow(a, b) | Shape::Line(a, b) => tiny(*a, *b),
            Shape::Rect(a, b) | Shape::Ellipse(a, b) | Shape::Mosaic(a, b) => {
                (a.0 - b.0).abs() < 2.0 || (a.1 - b.1).abs() < 2.0
            }
            Shape::Pen(pts) | Shape::Highlighter(pts) => {
                pts.len() < 2 || pts.iter().all(|p| tiny(*p, pts[0]))
            }
            Shape::Text { text, .. } => text.is_empty(),
            Shape::Badge { .. } => false,
        }
    }
}

// ---- コマンド列 ----

#[derive(Clone, Debug, PartialEq)]
enum Cmd {
    Add(usize, Annot),
    Remove(usize, Annot),
    /// items[i] を動かした量。
    Move(usize, P),
    /// 属性の変更・再編集 (前, 後)。
    Attr(usize, Annot, Annot),
    /// 複数の注釈への 1 手 (戻すときは逆の順)。
    Group(Vec<Cmd>),
}

/// 1 枚のピンの注釈。undo/redo は追加・移動・削除・属性変更のコマンド列。
#[derive(Default)]
pub struct Doc {
    pub items: Vec<Annot>,
    undo: Vec<Cmd>,
    redo: Vec<Cmd>,
    /// 振った番号の最大。消しても戻さない。
    badges: u32,
}

impl Doc {
    fn exec(&mut self, c: &Cmd, forward: bool) {
        match (c, forward) {
            (Cmd::Add(i, a), true) | (Cmd::Remove(i, a), false) => self.items.insert(*i, a.clone()),
            (Cmd::Add(i, _), false) | (Cmd::Remove(i, _), true) => {
                self.items.remove(*i);
            }
            (Cmd::Move(i, d), f) => {
                let k = if f { 1.0 } else { -1.0 };
                self.items[*i].translate((d.0 * k, d.1 * k));
            }
            (Cmd::Attr(i, old, new), f) => {
                self.items[*i] = if f { new.clone() } else { old.clone() }
            }
            (Cmd::Group(cs), true) => cs.iter().for_each(|c| self.exec(c, true)),
            (Cmd::Group(cs), false) => cs.iter().rev().for_each(|c| self.exec(c, false)),
        }
    }

    fn push(&mut self, c: Cmd) {
        self.exec(&c, true);
        self.record(c);
    }

    /// 幅 `w` の画像の左右反転: 全注釈と undo/redo の手を反転する。反転そのものは手として積まない。
    pub fn flip(&mut self, w: f32) {
        fn cmd(c: &mut Cmd, w: f32) {
            match c {
                Cmd::Add(_, a) | Cmd::Remove(_, a) => a.flip(w),
                Cmd::Move(_, d) => d.0 = -d.0,
                Cmd::Attr(_, a, b) => {
                    a.flip(w);
                    b.flip(w);
                }
                Cmd::Group(cs) => cs.iter_mut().for_each(|c| cmd(c, w)),
            }
        }
        self.items.iter_mut().for_each(|a| a.flip(w));
        self.undo
            .iter_mut()
            .chain(&mut self.redo)
            .for_each(|c| cmd(c, w));
    }

    /// 実行済みの手を積む。
    fn record(&mut self, c: Cmd) {
        self.undo.push(c);
        self.redo.clear();
    }

    /// 0 手なら積まず、1 手ならそのまま、2 手以上なら 1 手にまとめる。
    fn group(mut cs: Vec<Cmd>) -> Option<Cmd> {
        match cs.len() {
            0 => None,
            1 => cs.pop(),
            _ => Some(Cmd::Group(cs)),
        }
    }

    /// 足して、その添字を返す。
    pub fn add(&mut self, a: Annot) -> usize {
        let i = self.items.len();
        self.push(Cmd::Add(i, a));
        i
    }

    /// 末尾に 1 手で足して、その添字の範囲を返す (貼り付け)。
    pub fn add_all(&mut self, v: Vec<Annot>) -> std::ops::Range<usize> {
        let i = self.items.len();
        let cs = (i..).zip(v).map(|(k, a)| Cmd::Add(k, a)).collect();
        if let Some(c) = Self::group(cs) {
            self.push(c);
        }
        i..self.items.len()
    }

    /// `ids` の注釈を 1 手で消す。
    pub fn remove(&mut self, ids: &[usize]) {
        let mut ids = ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        // 後ろから消す: 前の添字がずれない。
        let cs = ids
            .iter()
            .rev()
            .filter_map(|&i| self.items.get(i).cloned().map(|a| Cmd::Remove(i, a)))
            .collect();
        if let Some(c) = Self::group(cs) {
            self.push(c);
        }
    }

    pub fn replace(&mut self, i: usize, new: Annot) {
        self.replace_all(vec![(i, new)]);
    }

    /// 属性の変更を 1 手で当てる (変わらない物は積まない)。
    pub fn replace_all(&mut self, news: Vec<(usize, Annot)>) {
        let cs = news
            .into_iter()
            .filter_map(|(i, new)| {
                self.items
                    .get(i)
                    .filter(|old| **old != new)
                    .map(|old| Cmd::Attr(i, old.clone(), new))
            })
            .collect();
        if let Some(c) = Self::group(cs) {
            self.push(c);
        }
    }

    /// ドラッグの途中で動かす (記録しない)。終わったら `moved` で合計を記録する。
    pub fn nudge(&mut self, i: usize, d: P) {
        if let Some(a) = self.items.get_mut(i) {
            a.translate(d);
        }
    }

    /// `nudge` で動かし終えた合計を 1 手で記録する (items はもう動いている)。
    pub fn moved(&mut self, ids: &[usize], total: P) {
        if total == (0.0, 0.0) {
            return;
        }
        let cs = ids
            .iter()
            .filter(|&&i| i < self.items.len())
            .map(|&i| Cmd::Move(i, total))
            .collect();
        if let Some(c) = Self::group(cs) {
            self.record(c);
        }
    }

    /// 元に戻す・やり直す。
    pub fn undo_redo(&mut self, redo: bool) -> bool {
        if redo { self.redo() } else { self.undo() }
    }

    pub fn undo(&mut self) -> bool {
        let Some(c) = self.undo.pop() else {
            return false;
        };
        self.exec(&c, false);
        self.redo.push(c);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(c) = self.redo.pop() else {
            return false;
        };
        self.exec(&c, true);
        self.undo.push(c);
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// 積んだ手の数。ホスト (GIF ピン) はこれで「エディタが 1 手積んだ」を知る。
    pub fn depth(&self) -> usize {
        self.undo.len()
    }

    /// 次に置くバッジの番号。バッジが 1 つも無ければ 1 から数え直す。あれば消した番号も使い直さない
    /// (元に戻すで戻ったバッジの番号より大きくする)。
    pub fn peek_badge(&self) -> u32 {
        let max = self
            .items
            .iter()
            .filter_map(|a| match a.shape {
                Shape::Badge { n, .. } => Some(n),
                _ => None,
            })
            .max();
        match max {
            None => 1,
            Some(m) => self.badges.max(m) + 1,
        }
    }

    /// 番号を振る (配置ごとに 1 つ進む)。
    pub fn take_badge(&mut self) -> u32 {
        self.badges = self.peek_badge();
        self.badges
    }

    /// 上にある物から当てる。`keep(i)` が偽の注釈 (GIF ピンの区間外で描かない物) には当てない。
    pub fn hit(&self, p: P, tol: f32, keep: impl Fn(usize) -> bool) -> Option<usize> {
        (0..self.items.len())
            .rev()
            .find(|&i| keep(i) && self.items[i].hit(p, tol))
    }
}

// ---- 画面 ⇄ 画像 ----

/// 画面の矩形 `img` (物理 px) に `w` × `h` の画像を拡縮して出しているときの、画面 → 画像。
pub fn to_image(img: RECT, w: u32, h: u32, p: P) -> P {
    let kx = (img.right - img.left) as f32 / w as f32;
    let ky = (img.bottom - img.top) as f32 / h as f32;
    ((p.0 - img.left as f32) / kx, (p.1 - img.top as f32) / ky)
}

pub fn to_screen(img: RECT, w: u32, h: u32, p: P) -> P {
    let kx = (img.right - img.left) as f32 / w as f32;
    let ky = (img.bottom - img.top) as f32 / h as f32;
    (img.left as f32 + p.0 * kx, img.top as f32 + p.1 * ky)
}

// ---- モザイク ----

/// 範囲を画像に切り詰めた整数の矩形 (左, 上, 右, 下)。空なら None。
fn mosaic_bounds(a: P, b: P, w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
    let r = norm(a, b);
    let c = |v: f32, max: u32| (v.round().max(0.0) as u32).min(max);
    let (l, t, r, b) = (c(r.0, w), c(r.1, h), c(r.2, w), c(r.3, h));
    (l < r && t < b).then_some((l, t, r, b))
}

/// 升の色 (列, 行, BGRA)。升は範囲の左上から `cell` × `cell` に切り、端の欠けた升は範囲の内側だけで平均する。
fn mosaic_cells(img: &Image, ((l, t, r, b), cell): Mosaic) -> (u32, u32, Vec<u8>) {
    let cols = (r - l).div_ceil(cell);
    let rows = (b - t).div_ceil(cell);
    let mut out = Vec::with_capacity((cols * rows * 4) as usize);
    for row in 0..rows {
        for col in 0..cols {
            let (x0, y0) = (l + col * cell, t + row * cell);
            let (x1, y1) = ((x0 + cell).min(r), (y0 + cell).min(b));
            let mut sum = [0u32; 3];
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = ((y * img.w + x) * 4) as usize;
                    for (s, v) in sum.iter_mut().zip(&img.bgra[i..i + 3]) {
                        *s += *v as u32;
                    }
                }
            }
            let n = (x1 - x0) * (y1 - y0);
            out.extend(sum.iter().map(|s| ((s + n / 2) / n) as u8));
            out.push(255);
        }
    }
    (cols, rows, out)
}

/// 元の画像 `src` から升を計算し、`out` の範囲を塗る。
fn apply_mosaic(out: &mut Image, src: &Image, m: Mosaic) {
    let (cols, _, cells) = mosaic_cells(src, m);
    paint_cells(out, m, cols, &cells);
}

/// `mosaic_cells` の升で `out` の範囲を塗る。
fn paint_cells(out: &mut Image, (r, cell): Mosaic, cols: u32, cells: &[u8]) {
    for y in r.1..r.3 {
        for x in r.0..r.2 {
            let c = (((y - r.1) / cell * cols + (x - r.0) / cell) * 4) as usize;
            let i = ((y * out.w + x) * 4) as usize;
            out.bgra[i..i + 4].copy_from_slice(&cells[c..c + 4]);
        }
    }
}

// ---- 合成 ----

/// コピーと保存の画像: 元の画像 + 全注釈。注釈が無ければ元の画像そのもの。
/// モザイクは元の画像から計算して先に塗り、ほかの注釈はその上に描く (画面の描き方と同じ順)。
pub fn composite(image: &Image, items: &[Annot]) -> Image {
    let mut out = image.clone();
    for m in mosaic_rects(items, image.w, image.h) {
        apply_mosaic(&mut out, image, m);
    }
    if items.iter().all(Annot::is_mosaic) {
        return out;
    }
    match render(out.w, out.h, Some(&out.bgra), items) {
        Ok(bgra) => Image { bgra, ..out },
        Err(e) => {
            eprintln!("annot: composite failed: {e}");
            out
        }
    }
}

// ---- GIF ピン ----

/// GIF ピンの注釈の区間: 注釈を出すフレーム (保存したフレームの添字)。両端を含む範囲の並びで、昇順で
/// 重ならず隣り合わない (飛び飛びにできる)。`Doc::items` と同じ並びで持つ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span(Vec<(usize, usize)>);

impl Span {
    /// `a`..=`b` の 1 本。
    pub fn new((a, b): (usize, usize)) -> Self {
        Span(vec![(a.min(b), a.max(b))])
    }

    pub fn ranges(&self) -> &[(usize, usize)] {
        &self.0
    }

    pub fn contains(&self, i: usize) -> bool {
        self.0.iter().any(|&(a, b)| a <= i && i <= b)
    }

    /// 最初の始めから最後の終わりまで。
    pub fn hull(&self) -> (usize, usize) {
        (self.0[0].0, self.0[self.0.len() - 1].1)
    }

    /// `a`..=`b` を足す (`on`) か外す。全部外れると区間が無くなるときは変えず false。
    pub fn set(&mut self, (a, b): (usize, usize), on: bool) -> bool {
        let (a, b) = (a.min(b), a.max(b));
        let mut v: Vec<(usize, usize)> = Vec::with_capacity(self.0.len() + 2);
        for &(x, y) in &self.0 {
            // 外す範囲の左右の残り。
            if x < a {
                v.push((x, y.min(a - 1)));
            }
            if y > b {
                v.push((x.max(b + 1), y));
            }
        }
        if on {
            v.push((a, b));
        }
        v.sort_unstable();
        let mut out: Vec<(usize, usize)> = Vec::with_capacity(v.len());
        for (x, y) in v {
            match out.last_mut() {
                Some(l) if x <= l.1 + 1 => l.1 = l.1.max(y),
                _ => out.push((x, y)),
            }
        }
        if out.is_empty() {
            return false;
        }
        self.0 = out;
        true
    }
}

/// GIF の「今のフレームをコピー」: フレーム `i` の画素 + そのフレームが実効の区間に入っている注釈。
pub fn composite_frame(
    image: &Image,
    items: &[Annot],
    spans: &[Span],
    tl: &Timeline,
    i: usize,
) -> Image {
    let on: Vec<Annot> = items
        .iter()
        .zip(spans)
        .filter(|(_, s)| tl.effective(s.ranges()).any(|j| j == i))
        .map(|(a, _)| a.clone())
        .collect();
    composite(image, &on)
}

/// 書き出しの区切り: `range` を区間の境目で切り、各組に出ている注釈の添字を返す。注釈の無い組は返さない。
/// 削除したフレームは書き出されないので、区間は生のまま使う。
pub fn segments(spans: &[Span], range: Range<usize>) -> Vec<(Range<usize>, Vec<usize>)> {
    let mut cuts: Vec<usize> = spans
        .iter()
        .flat_map(|s| s.ranges().iter().flat_map(|&(a, b)| [a, b + 1]))
        .chain([range.start, range.end])
        .filter(|c| range.contains(c) || *c == range.end)
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    cuts.windows(2)
        .filter_map(|w| {
            let on: Vec<usize> = (0..spans.len())
                .filter(|&k| spans[k].contains(w[0]))
                .collect();
            (!on.is_empty()).then(|| (w[0]..w[1], on))
        })
        .collect()
}

/// `w` × `h` の透明な層にモザイク以外の注釈を描く (premultiplied BGRA)。UI スレッド専用 (`ui::gfx()`)。
/// 描く物が無ければ None。
pub fn layer(w: u32, h: u32, items: &[Annot]) -> Option<Vec<u8>> {
    if items.iter().all(Annot::is_mosaic) {
        return None;
    }
    render(w, h, None, items)
        .inspect_err(|e| eprintln!("annot: layer failed: {e}"))
        .ok()
}

/// モザイクの 1 個: 画像に切り詰めた矩形と升の辺。
pub type Mosaic = (Ltrb, u32);

fn mosaic_of(a: &Annot, w: u32, h: u32) -> Option<Mosaic> {
    match a.shape {
        Shape::Mosaic(p, q) => mosaic_bounds(p, q, w, h).map(|r| (r, mosaic_cell(a.width))),
        _ => None,
    }
}

/// モザイクの矩形を画像に切り詰めたもの (書き出しへ渡す)。
pub fn mosaic_rects(items: &[Annot], w: u32, h: u32) -> Vec<Mosaic> {
    items.iter().filter_map(|a| mosaic_of(a, w, h)).collect()
}

/// 書き出しのフレームに注釈を重ねる。モザイクはこのフレームの画素から計算して先に塗り、
/// 層 (premultiplied) はその上にアルファで重ねる (`composite` と同じ順)。
/// 層は `bounds` (`layer_bounds` の外接矩形) の中だけを重ねる。外の画素は 4 バイトとも 0 で、重ねても変わらない。
pub fn overlay(img: &mut Image, layer: Option<(&[u8], Ltrb)>, mosaics: &[Mosaic]) {
    // 升は全部の矩形を塗る前に元の画素から計算する (重なった矩形も元の画素の升になる)。
    let cells: Vec<_> = mosaics.iter().map(|&m| mosaic_cells(img, m)).collect();
    for (&m, (cols, _, c)) in mosaics.iter().zip(&cells) {
        paint_cells(img, m, *cols, c);
    }
    if let Some((l, (x0, y0, x1, y1))) = layer {
        let stride = img.w as usize * 4;
        let (a, b) = (x0 as usize * 4, x1 as usize * 4);
        for y in y0 as usize..y1 as usize {
            let row = y * stride;
            let (dst, src) = (&mut img.bgra[row + a..row + b], &l[row + a..row + b]);
            for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                let k = 255 - u32::from(s[3]);
                for c in 0..4 {
                    d[c] = (u32::from(s[c]) + (u32::from(d[c]) * k + 127) / 255).min(255) as u8;
                }
            }
        }
    }
}

/// 層の 0 でない画素 (4 バイトのどれか) の外接矩形 (左, 上, 右, 下)。全部 0 なら None。
pub fn layer_bounds(layer: &[u8], w: u32) -> Option<Ltrb> {
    let stride = w as usize * 4;
    let (mut x0, mut x1, mut y0, mut y1) = (usize::MAX, 0, usize::MAX, 0);
    for (y, row) in layer.chunks_exact(stride).enumerate() {
        let lit = |p: &[u8]| p != [0; 4];
        let Some(a) = row.chunks_exact(4).position(lit) else {
            continue;
        };
        let b = row.chunks_exact(4).rposition(lit).unwrap_or(a);
        (x0, x1, y0, y1) = (x0.min(a), x1.max(b + 1), y0.min(y), y + 1);
    }
    (y0 != usize::MAX).then_some((x0 as u32, y0 as u32, x1 as u32, y1 as u32))
}

/// GIF ピンの undo の印の列。フレームの編集 (`Timeline`) と注釈 (`Doc` と区間) を 1 本にする。
/// 中身の手は `Timeline` と `Doc` が持ち、ここは「どちらの手か」と区間の前の値だけを持つ。
#[derive(Default)]
pub struct History {
    undo: Vec<Hist>,
    redo: Vec<Hist>,
}

enum Hist {
    /// `Timeline` の削除 (`tl.delete()` が true を返したときだけ積む)。
    Frames,
    /// `Doc` の 1 手と、その前の区間。
    Doc(Vec<Span>),
    /// 区間だけの変更と、その前の区間。
    Span(Vec<Span>),
}

impl History {
    fn push(&mut self, h: Hist) {
        self.undo.push(h);
        self.redo.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// フレームを削除した後に積む。
    pub fn frames(&mut self) {
        self.push(Hist::Frames);
    }

    /// `Doc` に 1 手を積んだ後に、その手の前の区間を渡して積む。
    pub fn doc(&mut self, before: Vec<Span>) {
        self.push(Hist::Doc(before));
    }

    /// 区間を変えた後に、前の区間を渡して積む。
    pub fn span(&mut self, before: Vec<Span>) {
        self.push(Hist::Span(before));
    }

    /// 印を 1 つ取り、その手を戻す (`redo` ならやり直す)。戻す物が無ければ false。
    pub fn undo_redo(
        &mut self,
        redo: bool,
        tl: &mut Timeline,
        doc: &mut Doc,
        spans: &mut Vec<Span>,
    ) -> bool {
        let (from, to) = if redo {
            (&mut self.redo, &mut self.undo)
        } else {
            (&mut self.undo, &mut self.redo)
        };
        let Some(h) = from.pop() else {
            return false;
        };
        let back = match h {
            Hist::Frames => {
                tl.undo_redo(redo);
                Hist::Frames
            }
            Hist::Doc(s) => {
                if redo {
                    doc.redo();
                } else {
                    doc.undo();
                }
                Hist::Doc(std::mem::replace(spans, s))
            }
            Hist::Span(s) => Hist::Span(std::mem::replace(spans, s)),
        };
        to.push(back);
        true
    }
}

/// モザイク以外の注釈を D2D で描き、CPU に読み戻す (premultiplied BGRA)。`base` が None なら透明な地。
fn render(w: u32, h: u32, base: Option<&[u8]>, items: &[Annot]) -> Result<Vec<u8>> {
    let size = D2D_SIZE_U {
        width: w,
        height: h,
    };
    let props = |options| D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: options,
        ..Default::default()
    };
    unsafe {
        let ctx = ui::gfx()
            .d2d
            .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
        ctx.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let target = ctx.CreateBitmap(size, None, 0, &props(D2D1_BITMAP_OPTIONS_TARGET))?;
        ctx.SetTarget(&target);
        ctx.BeginDraw();
        match base {
            Some(bgra) => {
                let bitmap = ui::bitmap_from_bgra(&ctx, w, h, bgra)?;
                ctx.DrawBitmap(
                    &bitmap,
                    None,
                    1.0,
                    D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                    None,
                    None,
                );
            }
            None => ctx.Clear(Some(&D2D1_COLOR_F::default())),
        }
        for a in items.iter().filter(|a| !a.is_mosaic()) {
            draw_vector(&ctx, a, 1.0);
        }
        ctx.EndDraw(None, None)?;
        let read = ctx.CreateBitmap(
            size,
            None,
            0,
            &props(D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW),
        )?;
        read.CopyFromBitmap(None, &target, None)?;
        let map = read.Map(D2D1_MAP_OPTIONS_READ)?;
        let row = w as usize * 4;
        let mut bgra = Vec::with_capacity(row * h as usize);
        for y in 0..h as usize {
            let src = std::slice::from_raw_parts(map.bits.add(y * map.pitch as usize), row);
            bgra.extend_from_slice(src);
        }
        read.Unmap()?;
        Ok(bgra)
    }
}

// ---- 描画 ----

pub fn ink(i: usize, alpha: f32) -> D2D1_COLOR_F {
    let mut c = ui::hex(if i == INK_GHOST { GHOST } else { INKS[i] });
    c.a = alpha;
    c
}

fn v2(p: P) -> Vector2 {
    Vector2 { X: p.0, Y: p.1 }
}

/// 升の色の bitmap の置き場。持ち主はピン (描画の呼び手)。鍵は (画像の番号, 範囲): 静止画ピンは画像が変わらないので
/// 番号はいつも 0、GIF ピンはフレームの番号。番号が変われば全部捨てる。
#[derive(Default)]
pub struct MosaicCache(RefCell<Vec<(MosaicKey, ID2D1Bitmap1)>>);

type MosaicKey = (usize, Mosaic);

/// ponytail: 範囲を引いている間は鍵が毎コマ変わる。古い物から捨てる固定の数。
const MOSAIC_CACHED: usize = 16;

impl MosaicCache {
    /// 鍵の bitmap。無ければ升を計算して作り、覚える。
    fn get(
        &self,
        ctx: &ID2D1DeviceContext,
        image: &Image,
        frame: usize,
        r: Mosaic,
    ) -> Option<ID2D1Bitmap1> {
        let mut v = self.0.borrow_mut();
        if v.first().is_some_and(|((f, _), _)| *f != frame) {
            v.clear();
        }
        if let Some((_, b)) = v.iter().find(|(k, _)| *k == (frame, r)) {
            return Some(b.clone());
        }
        let (cols, rows, cells) = mosaic_cells(image, r);
        let b = ui::bitmap_from_bgra(ctx, cols, rows, &cells).ok()?;
        if v.len() >= MOSAIC_CACHED {
            v.remove(0);
        }
        v.push(((frame, r), b.clone()));
        Some(b)
    }
}

/// 画面の描画: モザイクを先に、ほかをその上に。変換 (画像 → 面) は呼び手が置く。
/// `frame` は `image` の中身の番号 (`MosaicCache` の鍵)。
pub fn draw<'a>(
    ctx: &ID2D1DeviceContext,
    image: &Image,
    (cache, frame): (&MosaicCache, usize),
    items: impl Iterator<Item = &'a Annot> + Clone,
    alpha: f32,
) {
    for m in items.clone().filter_map(|a| mosaic_of(a, image.w, image.h)) {
        draw_mosaic(ctx, cache.get(ctx, image, frame, m), m, alpha);
    }
    for a in items.filter(|a| !a.is_mosaic()) {
        draw_vector(ctx, a, alpha);
    }
}

/// 升の色の小さな bitmap を、1 升 `cell` × `cell` にニアレストで引き伸ばし、範囲で切る。
fn draw_mosaic(
    ctx: &ID2D1DeviceContext,
    bitmap: Option<ID2D1Bitmap1>,
    (r, cell): Mosaic,
    alpha: f32,
) {
    crate::insight_scope!("annot_mosaic");
    let Some(bitmap) = bitmap else {
        return;
    };
    let (cols, rows) = ((r.2 - r.0).div_ceil(cell), (r.3 - r.1).div_ceil(cell));
    let (l, t) = (r.0 as f32, r.1 as f32);
    unsafe {
        ctx.PushAxisAlignedClip(
            &ui::rectf(l, t, r.2 as f32, r.3 as f32),
            D2D1_ANTIALIAS_MODE_ALIASED,
        );
        ctx.DrawBitmap(
            &bitmap,
            Some(&ui::rectf(
                l,
                t,
                l + (cols * cell) as f32,
                t + (rows * cell) as f32,
            )),
            alpha,
            D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            None,
            None,
        );
        ctx.PopAxisAlignedClip();
    }
}

fn polyline(pts: &[P]) -> Option<ID2D1PathGeometry1> {
    unsafe {
        let geo = ui::gfx().factory.CreatePathGeometry().ok()?;
        let sink = geo.Open().ok()?;
        sink.BeginFigure(v2(pts[0]), D2D1_FIGURE_BEGIN_HOLLOW);
        for p in &pts[1..] {
            sink.AddLine(v2(*p));
        }
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
        sink.Close().ok()?;
        Some(geo)
    }
}

fn draw_vector(ctx: &ID2D1DeviceContext, a: &Annot, alpha: f32) {
    let w = a.width;
    let round = ui::widgets::round_stroke();
    unsafe {
        let Ok(brush) = ctx.CreateSolidColorBrush(&ink(a.ink, alpha), None) else {
            return;
        };
        match &a.shape {
            Shape::Line(p, q) => ctx.DrawLine(v2(*p), v2(*q), &brush, w, &round),
            Shape::Arrow(p, q) => {
                ctx.DrawLine(v2(*p), v2(*q), &brush, w, &round);
                let (h1, h2) = arrow_head(*p, *q, w);
                if let Some(g) = polyline(&[h1, *q, h2]) {
                    ctx.DrawGeometry(&g, &brush, w, &round);
                }
            }
            Shape::Rect(p, q) => {
                let r = norm(*p, *q);
                let r = ui::rectf(r.0, r.1, r.2, r.3);
                if a.fill {
                    ctx.FillRectangle(&r, &brush);
                }
                ctx.DrawRectangle(&r, &brush, w, None);
            }
            Shape::Ellipse(p, q) => {
                let r = norm(*p, *q);
                let e = D2D1_ELLIPSE {
                    point: v2(((r.0 + r.2) / 2.0, (r.1 + r.3) / 2.0)),
                    radiusX: (r.2 - r.0) / 2.0,
                    radiusY: (r.3 - r.1) / 2.0,
                };
                if a.fill {
                    ctx.FillEllipse(&e, &brush);
                }
                ctx.DrawEllipse(&e, &brush, w, None);
            }
            Shape::Pen(pts) if pts.len() >= 2 => {
                if let Some(g) = polyline(pts) {
                    ctx.DrawGeometry(&g, &brush, w, &round);
                }
            }
            Shape::Highlighter(pts) if pts.len() >= 2 => {
                // 1 本の形として描くので、重なりが濃くならない。
                brush.SetOpacity(HIGHLIGHTER_OPACITY);
                if let Some(g) = polyline(pts) {
                    ctx.DrawGeometry(&g, &brush, w * 4.0, &round);
                }
            }
            Shape::Text {
                at,
                text,
                w: tw,
                h: th,
                deco,
            } => {
                let t = ui::ink_text(text, a.size);
                if *deco == Deco::None {
                    ui::draw_text(ctx, &t, at.0, at.1, ink(a.ink, alpha));
                    return;
                }
                // 縁と文字の重なりが透けないように、不透明に描いてから層で不透明度を掛ける。
                let layer = alpha < 1.0;
                if layer {
                    ctx.PushLayer(
                        &D2D1_LAYER_PARAMETERS1 {
                            contentBounds: ui::rectf(f32::MIN, f32::MIN, f32::MAX, f32::MAX),
                            geometricMask: std::mem::ManuallyDrop::new(None),
                            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                            maskTransform: windows_numerics::Matrix3x2::identity(),
                            opacity: alpha,
                            opacityBrush: std::mem::ManuallyDrop::new(None),
                            layerOptions: D2D1_LAYER_OPTIONS1_NONE,
                        },
                        None,
                    );
                }
                let (rim, fg) = (ink(deco_ink(a.ink), 1.0), ink(a.ink, 1.0));
                let outline = (*deco == Deco::Outline)
                    .then(|| ui::text_outline(&t, at.0, at.1))
                    .flatten();
                match (outline, ctx.CreateSolidColorBrush(&rim, None)) {
                    (Some(g), Ok(rb)) => {
                        // 線の芯は輪郭なので、太さの倍で引くと外へ縁取りの太さだけ出る。
                        ctx.DrawGeometry(&g, &rb, outline_w(a.size) * 2.0, &round);
                        brush.SetColor(&fg);
                        ctx.FillGeometry(&g, &brush, None);
                    }
                    _ => {
                        if *deco == Deco::Box {
                            let (dx, dy) = a.deco_pad();
                            ui::fill_round(
                                ctx,
                                ui::rectf(at.0 - dx, at.1 - dy, at.0 + tw + dx, at.1 + th + dy),
                                (a.size / 6.0).round(),
                                rim,
                            );
                        }
                        ui::draw_text(ctx, &t, at.0, at.1, fg);
                    }
                }
                if layer {
                    ctx.PopLayer();
                }
            }
            Shape::Badge { at, n } => {
                let d = a.badge_d();
                let rim = if a.ink == INK_WHITE {
                    INK_BLACK
                } else {
                    INK_WHITE
                };
                let disc = |r: f32, c: D2D1_COLOR_F| {
                    if let Ok(b) = ctx.CreateSolidColorBrush(&c, None) {
                        ctx.FillEllipse(
                            &D2D1_ELLIPSE {
                                point: v2(*at),
                                radiusX: r,
                                radiusY: r,
                            },
                            &b,
                        );
                    }
                };
                disc(d / 2.0 + 2.0, ink(rim, alpha));
                disc(d / 2.0, ink(a.ink, alpha));
                let t = ui::badge_text(&n.to_string(), d / 2.0);
                let num = if light_ink(a.ink) {
                    ink(INK_BLACK, alpha)
                } else {
                    ink(INK_WHITE, alpha)
                };
                ui::draw_text(ctx, &t, at.0 - t.w / 2.0, at.1 - t.h / 2.0, num);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(a: P, b: P) -> Annot {
        Annot {
            shape: Shape::Rect(a, b),
            ink: INK_RED,
            width: 4.0,
            size: 18.0,
            fill: false,
        }
    }

    fn badge(doc: &mut Doc, at: P) -> usize {
        let n = doc.take_badge();
        doc.add(Annot {
            shape: Shape::Badge { at, n },
            ink: INK_RED,
            width: 4.0,
            size: 18.0,
            fill: false,
        })
    }

    fn numbers(doc: &Doc) -> Vec<u32> {
        doc.items
            .iter()
            .filter_map(|a| match a.shape {
                Shape::Badge { n, .. } => Some(n),
                _ => None,
            })
            .collect()
    }

    /// 左右反転 (幅 100): 点は x → 100 − x。テキストは箱の左上が元の右上の鏡、バッジは中心。2 回で元に戻る。
    /// 反転の前に積んだ移動と削除の undo も、反転後の座標で戻る。
    #[test]
    fn flip_table() {
        let with = |shape| Annot {
            shape,
            ..rect((0.0, 0.0), (0.0, 0.0))
        };
        let text = |x| Shape::Text {
            at: (x, 5.0),
            text: "A".into(),
            w: 30.0,
            h: 20.0,
            deco: Deco::Box,
        };
        let rows = [
            (
                Shape::Arrow((10.0, 1.0), (40.0, 2.0)),
                Shape::Arrow((90.0, 1.0), (60.0, 2.0)),
            ),
            (
                Shape::Rect((10.0, 1.0), (40.0, 2.0)),
                Shape::Rect((90.0, 1.0), (60.0, 2.0)),
            ),
            (
                Shape::Pen(vec![(0.0, 1.0), (25.0, 3.0)]),
                Shape::Pen(vec![(100.0, 1.0), (75.0, 3.0)]),
            ),
            (text(10.0), text(60.0)),
            (
                Shape::Badge {
                    at: (20.0, 7.0),
                    n: 1,
                },
                Shape::Badge {
                    at: (80.0, 7.0),
                    n: 1,
                },
            ),
        ];
        for (from, to) in rows {
            let mut a = with(from.clone());
            a.flip(100.0);
            assert_eq!(a.shape, to, "from {from:?}");
            a.flip(100.0);
            assert_eq!(a.shape, from, "twice");
        }
        // 履歴: 足して動かして消した後に反転し、3 手戻すと反転後の座標の元の位置。
        let mut doc = Doc::default();
        doc.add(rect((10.0, 0.0), (20.0, 10.0)));
        doc.nudge(0, (5.0, 0.0));
        doc.moved(&[0], (5.0, 0.0));
        doc.remove(&[0]);
        doc.flip(100.0);
        assert!(doc.undo());
        assert_eq!(doc.items[0].shape, Shape::Rect((85.0, 0.0), (75.0, 10.0)));
        assert!(doc.undo());
        assert_eq!(doc.items[0].shape, Shape::Rect((90.0, 0.0), (80.0, 10.0)));
        assert!(doc.redo());
        assert_eq!(doc.items[0].shape, Shape::Rect((85.0, 0.0), (75.0, 10.0)));
    }

    /// ハンドルの拡縮: 箱から箱へ写すと、矩形は角が、ペンは各点が比で動く。幅 0 の向きは平行移動だけ。
    #[test]
    fn fit_maps_box_to_box() {
        let mut r = rect((10.0, 10.0), (50.0, 40.0));
        r.fit((10.0, 10.0, 50.0, 40.0), (10.0, 10.0, 90.0, 25.0));
        assert_eq!(r.shape, Shape::Rect((10.0, 10.0), (90.0, 25.0)));
        let mut pen = Annot {
            shape: Shape::Pen(vec![(0.0, 5.0), (10.0, 5.0)]),
            ..rect((0.0, 0.0), (0.0, 0.0))
        };
        let from = pen.core_box().unwrap();
        assert_eq!(from, (0.0, 5.0, 10.0, 5.0));
        pen.fit(from, (0.0, 8.0, 20.0, 8.0));
        assert_eq!(pen.shape, Shape::Pen(vec![(0.0, 8.0), (20.0, 8.0)]));
    }

    #[test]
    fn undo_redo_command_list() {
        let mut doc = Doc::default();
        let a = rect((10.0, 10.0), (50.0, 40.0));
        let i = doc.add(a.clone());
        doc.nudge(i, (5.0, 7.0));
        doc.moved(&[i], (5.0, 7.0));
        let moved = doc.items[i].clone();
        assert_eq!(moved.shape, Shape::Rect((15.0, 17.0), (55.0, 47.0)));
        doc.remove(&[i]);
        assert!(doc.items.is_empty());
        assert!(doc.undo()); // 削除を戻す
        assert_eq!(doc.items, vec![moved.clone()]);
        assert!(doc.undo()); // 移動を戻す
        assert_eq!(doc.items, vec![a.clone()]);
        assert!(doc.undo()); // 追加を戻す
        assert!(doc.items.is_empty());
        assert!(!doc.can_undo() && doc.can_redo());
        assert!(doc.redo()); // 追加をやり直す
        assert_eq!(doc.items, vec![a]);
        assert!(doc.can_undo() && doc.can_redo());
        // 新しい操作でやり直しの列は消える。
        doc.replace(0, rect((0.0, 0.0), (9.0, 9.0)));
        assert!(!doc.can_redo());
    }

    /// 塗った四角・丸は中も当たる。塗らなければ線の上だけ。
    #[test]
    fn filled_hits_inside() {
        for shape in [
            Shape::Rect((0.0, 0.0), (100.0, 60.0)),
            Shape::Ellipse((0.0, 0.0), (100.0, 60.0)),
        ] {
            for fill in [false, true] {
                let a = Annot {
                    shape: shape.clone(),
                    fill,
                    ..rect((0.0, 0.0), (0.0, 0.0))
                };
                assert_eq!(a.hit((50.0, 30.0), 0.0), fill, "{shape:?} {fill}");
            }
        }
    }

    #[test]
    fn hit_test_under_zoom() {
        // 200 × 100 の画像を 2.5 倍、画面の (100, 50) に出している。
        let img = RECT {
            left: 100,
            top: 50,
            right: 600,
            bottom: 300,
        };
        let mut doc = Doc::default();
        doc.add(rect((40.0, 20.0), (120.0, 80.0)));
        // 画像 ⇄ 画面の往復。
        for p in [(0.0, 0.0), (40.0, 20.0), (199.0, 99.0), (12.5, 3.25)] {
            let back = to_image(img, 200, 100, to_screen(img, 200, 100, p));
            assert!((back.0 - p.0).abs() < 1e-4 && (back.1 - p.1).abs() < 1e-4);
        }
        // 矩形の左辺 (画像の x = 40) は画面の x = 200。そこを押すと当たる。
        let on_edge = to_image(img, 200, 100, (200.0, 175.0));
        assert_eq!(doc.hit(on_edge, 4.0 / 2.5, |_| true), Some(0));
        // 線から離れた内側と、等倍のときの位置 (140, 70) は当たらない。
        assert_eq!(
            doc.hit(to_image(img, 200, 100, (300.0, 175.0)), 1.6, |_| true),
            None
        );
        assert_eq!(
            doc.hit(to_image(img, 200, 100, (140.0, 70.0)), 1.6, |_| true),
            None
        );
    }

    fn gradient(w: u32, h: u32) -> Image {
        let mut bgra = Vec::new();
        for y in 0..h {
            for x in 0..w {
                bgra.extend([
                    (x * 7 % 256) as u8,
                    (y * 13 % 256) as u8,
                    ((x ^ y) * 5 % 256) as u8,
                    255,
                ]);
            }
        }
        Image { w, h, bgra }
    }

    #[test]
    fn composite_mosaic_hides_pixels() {
        let img = gradient(64, 48);
        // 範囲は (10, 6)〜(35, 30): 升は範囲の左上から 線幅 × 2 の正方形、右と下の端は欠けた升。
        for (width, cell) in [(4.0, 8), (8.0, 16)] {
            let m = Annot {
                shape: Shape::Mosaic((35.0, 30.0), (10.0, 6.0)),
                ink: INK_RED,
                width,
                size: 18.0,
                fill: false,
            };
            let out = composite(&img, &[m]);
            let px = |im: &Image, x: u32, y: u32| {
                let i = ((y * im.w + x) * 4) as usize;
                im.bgra[i..i + 4].to_vec()
            };
            let mut changed = 0;
            for y in 6..30 {
                for x in 10..35 {
                    changed += (px(&out, x, y) != px(&img, x, y)) as u32;
                    // 1 升の中は 1 色 (升の左上と同じ)。
                    let (cx, cy) = (10 + (x - 10) / cell * cell, 6 + (y - 6) / cell * cell);
                    assert_eq!(px(&out, x, y), px(&out, cx, cy), "width {width}");
                }
            }
            // 範囲の画素は元とほぼ全部ちがう (平均と偶然一致する画素だけ残る)。
            assert!(changed > 25 * 24 * 9 / 10, "changed {changed}");
            // 範囲の外は元のまま。
            for (x, y) in [(9, 6), (35, 29), (10, 5), (20, 30), (0, 0), (63, 47)] {
                assert_eq!(px(&out, x, y), px(&img, x, y));
            }
        }
    }

    #[test]
    fn composite_without_annotations_is_original() {
        let img = gradient(31, 17);
        let out = composite(&img, &[]);
        assert_eq!((out.w, out.h), (img.w, img.h));
        assert!(out.bgra == img.bgra);
    }

    #[test]
    fn badge_numbers_never_reused() {
        let mut doc = Doc::default();
        for k in 0..3 {
            badge(&mut doc, (10.0 + 30.0 * k as f32, 10.0));
        }
        assert_eq!(numbers(&doc), vec![1, 2, 3]);
        doc.remove(&[1]);
        assert_eq!(numbers(&doc), vec![1, 3]);
        badge(&mut doc, (200.0, 10.0));
        assert_eq!(numbers(&doc), vec![1, 3, 4]);
        // 全部消すと 1 から。元に戻すで 1, 3, 4 が戻っても、次は 5 (番号は重ならない)。
        doc.remove(&[0, 1, 2]);
        badge(&mut doc, (10.0, 10.0));
        assert_eq!(numbers(&doc), vec![1]);
        assert!(doc.undo() && doc.undo());
        assert_eq!(numbers(&doc), vec![1, 3, 4]);
        badge(&mut doc, (10.0, 10.0));
        assert_eq!(numbers(&doc), vec![1, 3, 4, 5]);
    }

    #[test]
    fn group_remove_and_move_are_one_step() {
        // 矩形選択で 0 と 2 を選んで動かし、消す: どちらも 1 手で、戻すと元の並びに戻る。
        let mut doc = Doc::default();
        let items: Vec<Annot> = (0..3)
            .map(|k| rect((k as f32 * 20.0, 0.0), (k as f32 * 20.0 + 10.0, 10.0)))
            .collect();
        for a in &items {
            doc.add(a.clone());
        }
        let before = doc.depth();
        for i in [0, 2] {
            doc.nudge(i, (0.0, 5.0));
        }
        doc.moved(&[0, 2], (0.0, 5.0));
        doc.remove(&[2, 0]);
        assert_eq!(doc.depth(), before + 2);
        assert_eq!(doc.items, vec![items[1].clone()]);
        assert!(doc.undo());
        assert!(doc.undo());
        assert_eq!(doc.items, items);
    }

    #[test]
    fn arrow_head_shape() {
        // 線幅 4: 長さ 22、片側 26°。右向きの矢印の頭は終点から左上と左下へ。
        let (p1, p2) = arrow_head((0.0, 0.0), (100.0, 0.0), 4.0);
        let len = |p: P| ((p.0 - 100.0).powi(2) + p.1.powi(2)).sqrt();
        assert!((len(p1) - 22.0).abs() < 1e-3 && (len(p2) - 22.0).abs() < 1e-3);
        let ang = |p: P| (p.1.abs() / (100.0 - p.0)).atan().to_degrees();
        assert!((ang(p1) - 26.0).abs() < 1e-3 && (ang(p2) - 26.0).abs() < 1e-3);
        assert!(p1.1 * p2.1 < 0.0);
    }

    #[test]
    fn span_toggles_frames() {
        let mut s = Span::new((0, 9));
        // 途中を外すと 2 本、隣を足すとつながる。
        assert!(s.set((3, 4), false));
        assert_eq!(s.ranges(), &[(0, 2), (5, 9)]);
        assert!(s.contains(2) && !s.contains(3) && s.contains(5));
        assert!(s.set((10, 12), true));
        assert_eq!(s.ranges(), &[(0, 2), (5, 12)]);
        assert!(s.set((3, 4), true));
        assert_eq!(s.ranges(), &[(0, 12)]);
        assert_eq!(s.hull(), (0, 12));
        // 全部は外せない。
        assert!(!s.set((0, 20), false));
        assert_eq!(s.ranges(), &[(0, 12)]);
    }

    #[test]
    fn history_tags_undo_in_action_order() {
        let ts = (0..30)
            .map(|i| std::time::Duration::from_millis(i * 33))
            .collect();
        let mut tl = Timeline::new(ts, std::time::Duration::from_millis(990));
        let (mut doc, mut spans, mut hist) =
            (Doc::default(), Vec::<Span>::new(), History::default());
        // フレームの削除 → 注釈の追加。
        tl.seek(3);
        assert!(tl.delete());
        hist.frames();
        let before = spans.clone();
        doc.add(rect((1.0, 1.0), (9.0, 9.0)));
        spans.push(Span::new((0, 29)));
        hist.doc(before);
        // undo 1 回目は注釈の追加を戻し、削除は残る。
        assert!(hist.undo_redo(false, &mut tl, &mut doc, &mut spans));
        assert!(doc.items.is_empty() && spans.is_empty());
        assert!(tl.deleted.contains(&3));
        // 2 回目で削除が戻る。
        assert!(hist.undo_redo(false, &mut tl, &mut doc, &mut spans));
        assert!(tl.deleted.is_empty());
        assert!(!hist.undo_redo(false, &mut tl, &mut doc, &mut spans));
        // redo は同じ順に進む: 削除、注釈の追加。
        assert!(hist.undo_redo(true, &mut tl, &mut doc, &mut spans));
        assert!(tl.deleted.contains(&3) && doc.items.is_empty());
        assert!(hist.undo_redo(true, &mut tl, &mut doc, &mut spans));
        assert_eq!(
            (doc.items.len(), spans.as_slice()),
            (1, &[Span::new((0, 29))][..])
        );
        // 区間だけの変更も 1 手。新しい手を積むと redo は消える。
        let before = spans.clone();
        spans[0] = Span::new((5, 9));
        hist.span(before);
        assert!(hist.undo_redo(false, &mut tl, &mut doc, &mut spans));
        assert_eq!(spans, vec![Span::new((0, 29))]);
        hist.frames();
        assert!(!hist.undo_redo(true, &mut tl, &mut doc, &mut spans));
    }

    /// 縁取り・背景の色: 赤・緑・青・黒の文字に白、橙・黄・白の文字に黒 (DS の表)。入力中の EDIT の地 (`light_ink`) と同じ分け方。
    #[test]
    fn deco_ink_table() {
        let want = [
            INK_WHITE, INK_BLACK, INK_BLACK, INK_WHITE, INK_WHITE, INK_WHITE, INK_BLACK,
        ];
        for (i, w) in want.iter().enumerate() {
            assert_eq!(deco_ink(i), *w, "ink {i} {:06X}", INKS[i]);
        }
        for i in 0..=INK_GHOST {
            assert_eq!(deco_ink(i) == INK_BLACK, light_ink(i), "ink {i}");
        }
    }

    /// 装飾付きテキストの外接矩形 (選択の枠と当たり判定): 縁取りは max(2, round(size ÷ 8))、
    /// 背景は左右 round(size ÷ 4)・上下 round(size ÷ 12) だけ文字の箱より広い。
    #[test]
    fn deco_bbox_table() {
        let text = |deco, size| Annot {
            shape: Shape::Text {
                at: (100.0, 50.0),
                text: "A".into(),
                w: 40.0,
                h: 20.0,
                deco,
            },
            ink: INK_RED,
            width: 4.0,
            size,
            fill: false,
        };
        for (deco, size, (dx, dy)) in [
            (Deco::None, 18.0, (0.0, 0.0)),
            (Deco::Outline, 14.0, (2.0, 2.0)),
            (Deco::Outline, 32.0, (4.0, 4.0)),
            (Deco::Box, 18.0, (5.0, 2.0)),
            (Deco::Box, 32.0, (8.0, 3.0)),
        ] {
            let a = text(deco, size);
            assert_eq!(
                a.bbox(),
                (100.0 - dx, 50.0 - dy, 140.0 + dx, 70.0 + dy),
                "{deco:?} {size}"
            );
            // 箱の左の外 1 px: 装飾があれば当たる。
            assert_eq!(a.hit((99.0, 60.0), 0.0), dx >= 1.0, "{deco:?} {size}");
        }
    }
}
