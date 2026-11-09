//! ピンの注釈: オブジェクト、コマンド列の undo/redo、ヒットテスト、モザイク、描画と合成。
//! GIF ピンの注釈の区間・undo の印の列・書き出しの層もここ (区間は `Annot` の外、GIF ピン側で持つ)。
//! 座標と寸法は画像の物理 px。規則は DS「Odile」の `AnnotationToolbar`。

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::core::Result;
use windows_numerics::Vector2;

use std::ops::Range;

use crate::capture::Image;
use crate::timeline::Timeline;
use crate::ui;

pub type P = (f32, f32);

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
/// モザイクの 1 升 (画像の物理 px)。
const CELL: u32 = 8;

/// 明るい色 (orange・yellow・white)。番号は `ink-on-light`、EDIT の地は `ink-black`。
pub fn light_ink(ink: usize) -> bool {
    matches!(ink, 1 | 2 | 6 | INK_GHOST)
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
    /// `at` は文字の箱の左上。`w`・`h` は確定したときに測った箱。
    Text {
        at: P,
        text: String,
        w: f32,
        h: f32,
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

    /// 外接矩形 (線の太さを含む)。
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
            Shape::Text { at, w, h, .. } => (at.0, at.1, at.0 + w, at.1 + h),
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
                inside(grow(r, half), p) && !inside(grow(r, -half), p)
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
                within(rx + half, ry + half) && !within(rx - half, ry - half)
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

    /// 元に戻す・やり直す。動かしている途中 (`drag` = 添字と `nudge` の合計) なら、先にその移動を
    /// 記録してドラッグを終える (`drag` を空にする)。残すと、戻した後の別の注釈へ移動が足し込まれる。
    pub fn undo_redo(&mut self, redo: bool, drag: &mut Option<(usize, P)>) -> bool {
        if let Some((i, total)) = drag.take() {
            self.moved(&[i], total);
        }
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

    /// 次に置くバッジの番号。
    pub fn peek_badge(&self) -> u32 {
        self.badges + 1
    }

    /// 番号を振る (配置ごとに 1 つ進む)。
    pub fn take_badge(&mut self) -> u32 {
        self.badges += 1;
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

/// 升の色 (列, 行, BGRA)。升は範囲の左上から 8 × 8 に切り、端の欠けた升は範囲の内側だけで平均する。
fn mosaic_cells(img: &Image, (l, t, r, b): (u32, u32, u32, u32)) -> (u32, u32, Vec<u8>) {
    let cols = (r - l).div_ceil(CELL);
    let rows = (b - t).div_ceil(CELL);
    let mut out = Vec::with_capacity((cols * rows * 4) as usize);
    for row in 0..rows {
        for col in 0..cols {
            let (x0, y0) = (l + col * CELL, t + row * CELL);
            let (x1, y1) = ((x0 + CELL).min(r), (y0 + CELL).min(b));
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
fn apply_mosaic(out: &mut Image, src: &Image, r: (u32, u32, u32, u32)) {
    let (cols, _, cells) = mosaic_cells(src, r);
    for y in r.1..r.3 {
        for x in r.0..r.2 {
            let c = (((y - r.1) / CELL * cols + (x - r.0) / CELL) * 4) as usize;
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
    for a in items {
        if let Shape::Mosaic(p, q) = a.shape
            && let Some(r) = mosaic_bounds(p, q, image.w, image.h)
        {
            apply_mosaic(&mut out, image, r);
        }
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

/// GIF ピンの注釈の区間: 保存したフレームの添字 (始め, 終わり)、両端を含む。`Doc::items` と同じ並びで持つ。
pub type Span = (usize, usize);

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
        .filter(|(_, s)| tl.effective(**s).any(|j| j == i))
        .map(|(a, _)| a.clone())
        .collect();
    composite(image, &on)
}

/// 書き出しの区切り: `range` を区間の境目で切り、各組に出ている注釈の添字を返す。注釈の無い組は返さない。
/// 削除したフレームは書き出されないので、区間は生のまま使う。
pub fn segments(spans: &[Span], range: Range<usize>) -> Vec<(Range<usize>, Vec<usize>)> {
    let mut cuts: Vec<usize> = spans
        .iter()
        .flat_map(|&(a, b)| [a, b + 1])
        .chain([range.start, range.end])
        .filter(|c| range.contains(c) || *c == range.end)
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    cuts.windows(2)
        .filter_map(|w| {
            let on: Vec<usize> = (0..spans.len())
                .filter(|&k| spans[k].0 <= w[0] && w[0] <= spans[k].1)
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

/// モザイクの矩形を画像に切り詰めたもの (書き出しへ渡す)。
pub fn mosaic_rects(items: &[Annot], w: u32, h: u32) -> Vec<(u32, u32, u32, u32)> {
    items
        .iter()
        .filter_map(|a| match a.shape {
            Shape::Mosaic(p, q) => mosaic_bounds(p, q, w, h),
            _ => None,
        })
        .collect()
}

/// 書き出しのフレームに注釈を重ねる。モザイクはこのフレームの画素から計算して先に塗り、
/// 層 (premultiplied) はその上にアルファで重ねる (`composite` と同じ順)。
pub fn overlay(img: &mut Image, layer: Option<&[u8]>, mosaics: &[(u32, u32, u32, u32)]) {
    if !mosaics.is_empty() {
        let src = img.clone();
        for &r in mosaics {
            apply_mosaic(img, &src, r);
        }
    }
    if let Some(l) = layer {
        for (d, s) in img.bgra.chunks_exact_mut(4).zip(l.chunks_exact(4)) {
            let k = 255 - u32::from(s[3]);
            for c in 0..4 {
                d[c] = (u32::from(s[c]) + (u32::from(d[c]) * k + 127) / 255).min(255) as u8;
            }
        }
    }
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

/// 画面の描画: モザイクを先に、ほかをその上に。変換 (画像 → 面) は呼び手が置く。
pub fn draw<'a>(
    ctx: &ID2D1DeviceContext,
    image: &Image,
    items: impl Iterator<Item = &'a Annot> + Clone,
    alpha: f32,
) {
    for a in items.clone() {
        if let Shape::Mosaic(p, q) = a.shape
            && let Some(r) = mosaic_bounds(p, q, image.w, image.h)
        {
            draw_mosaic(ctx, image, r, alpha);
        }
    }
    for a in items.filter(|a| !a.is_mosaic()) {
        draw_vector(ctx, a, alpha);
    }
}

/// 升の色の小さな bitmap を、1 升 8 × 8 にニアレストで引き伸ばし、範囲で切る。
fn draw_mosaic(ctx: &ID2D1DeviceContext, image: &Image, r: (u32, u32, u32, u32), alpha: f32) {
    let (cols, rows, cells) = mosaic_cells(image, r);
    let Ok(bitmap) = ui::bitmap_from_bgra(ctx, cols, rows, &cells) else {
        return;
    };
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
                l + (cols * CELL) as f32,
                t + (rows * CELL) as f32,
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
                ctx.DrawRectangle(&ui::rectf(r.0, r.1, r.2, r.3), &brush, w, None);
            }
            Shape::Ellipse(p, q) => {
                let r = norm(*p, *q);
                ctx.DrawEllipse(
                    &D2D1_ELLIPSE {
                        point: v2(((r.0 + r.2) / 2.0, (r.1 + r.3) / 2.0)),
                        radiusX: (r.2 - r.0) / 2.0,
                        radiusY: (r.3 - r.1) / 2.0,
                    },
                    &brush,
                    w,
                    None,
                );
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
            Shape::Text { at, text, .. } => {
                let t = ui::ink_text(text, a.size);
                ui::draw_text(ctx, &t, at.0, at.1, ink(a.ink, alpha));
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
        }
    }

    fn badge(doc: &mut Doc, at: P) -> usize {
        let n = doc.take_badge();
        doc.add(Annot {
            shape: Shape::Badge { at, n },
            ink: INK_RED,
            width: 4.0,
            size: 18.0,
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

    #[test]
    fn undo_while_dragging_settles_the_move() {
        // 矩形を描き、押して 50 動かし (未記録)、押したまま Ctrl+Z → Ctrl+Y → さらに 20 → 離す → Ctrl+Z。
        let mut doc = Doc::default();
        let a = rect((10.0, 10.0), (50.0, 40.0));
        let i = doc.add(a.clone());
        // ピンの `Drag::Move` と同じ: 動かすたびに nudge して合計を持つ。
        let mut drag = Some((i, (0.0, 0.0)));
        let step = |doc: &mut Doc, drag: &mut Option<(usize, P)>, dx: f32| {
            if let Some((i, total)) = drag {
                doc.nudge(*i, (dx, 0.0));
                total.0 += dx;
            }
        };
        step(&mut doc, &mut drag, 50.0);
        assert!(doc.undo_redo(false, &mut drag));
        assert_eq!(doc.items, vec![a.clone()]);
        assert!(doc.undo_redo(true, &mut drag));
        step(&mut doc, &mut drag, 20.0);
        if let Some((i, total)) = drag.take() {
            doc.moved(&[i], total);
        }
        assert!(doc.undo_redo(false, &mut drag));
        assert_eq!(doc.items, vec![a]);
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
        // 範囲は (10, 6)〜(35, 30): 升は範囲の左上から 8 × 8、右と下の端は欠けた升。
        let m = Annot {
            shape: Shape::Mosaic((35.0, 30.0), (10.0, 6.0)),
            ink: INK_RED,
            width: 4.0,
            size: 18.0,
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
                let (cx, cy) = (10 + (x - 10) / 8 * 8, 6 + (y - 6) / 8 * 8);
                assert_eq!(px(&out, x, y), px(&out, cx, cy));
            }
        }
        // 範囲の画素は元とほぼ全部ちがう (平均と偶然一致する画素だけ残る)。
        assert!(changed > 25 * 24 * 9 / 10, "changed {changed}");
        // 範囲の外は元のまま。
        for (x, y) in [(9, 6), (35, 29), (10, 5), (20, 30), (0, 0), (63, 47)] {
            assert_eq!(px(&out, x, y), px(&img, x, y));
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
        spans.push((0, 29));
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
        assert_eq!((doc.items.len(), spans.as_slice()), (1, &[(0, 29)][..]));
        // 区間だけの変更も 1 手。新しい手を積むと redo は消える。
        let before = spans.clone();
        spans[0] = (5, 9);
        hist.span(before);
        assert!(hist.undo_redo(false, &mut tl, &mut doc, &mut spans));
        assert_eq!(spans, vec![(0, 29)]);
        hist.frames();
        assert!(!hist.undo_redo(true, &mut tl, &mut doc, &mut spans));
    }
}
