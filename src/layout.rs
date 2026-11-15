//! 窓を使わない置き方の計算: 辺の吸着、ピンの窓の矩形、ツールバーとラベルの置き方。
//! 座標は仮想画面の物理 px。

use windows::Win32::Foundation::{POINT, RECT};

use crate::ui::{px, stroke};

/// 吸着先の辺。縦の辺なら `pos` が x で、`from..to` が y の範囲 (横の辺はその逆)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub pos: i32,
    pub from: i32,
    pub to: i32,
}

/// 座標 `p` (辺に沿う向きの座標は `along`) を、しきい値 `th` 以内でいちばん近い辺へ吸着させる。
/// 辺の範囲から `th` より外れた所では効かない (遠くの窓の延長線には付かない)。
pub fn snap(p: i32, along: i32, edges: &[Edge], th: i32) -> Option<Edge> {
    edges
        .iter()
        .filter(|e| along >= e.from - th && along <= e.to + th)
        .map(|e| ((e.pos - p).abs(), *e))
        .filter(|(d, _)| *d <= th)
        .min_by_key(|(d, _)| *d)
        .map(|(_, e)| e)
}

/// 矩形 (Z 順、手前から) の辺のうち、点 `p` に近い所が手前の矩形に隠れていないもの。(縦の辺, 横の辺)
/// 辺の上で `p` にいちばん近い点が、手前の矩形の内側 (辺そのものは含まない) にあれば隠れている。
pub fn front_edges(rects: &[RECT], p: POINT) -> (Vec<Edge>, Vec<Edge>) {
    let hidden = |front: &[RECT], x: i32, y: i32| {
        front
            .iter()
            .any(|f| f.left < x && x < f.right && f.top < y && y < f.bottom)
    };
    let mut v = Vec::new();
    let mut h = Vec::new();
    for (i, r) in rects.iter().enumerate() {
        let front = &rects[..i];
        let y = p.y.clamp(r.top, r.bottom);
        let x = p.x.clamp(r.left, r.right);
        for pos in [r.left, r.right] {
            if !hidden(front, pos, y) {
                v.push(Edge {
                    pos,
                    from: r.top,
                    to: r.bottom,
                });
            }
        }
        for pos in [r.top, r.bottom] {
            if !hidden(front, x, pos) {
                h.push(Edge {
                    pos,
                    from: r.left,
                    to: r.right,
                });
            }
        }
    }
    (v, h)
}

/// 2 点を対角とする矩形 (右と下は含まない)。
pub fn rect_of(a: POINT, b: POINT) -> RECT {
    RECT {
        left: a.x.min(b.x),
        top: a.y.min(b.y),
        right: a.x.max(b.x),
        bottom: a.y.max(b.y),
    }
}

/// ピンの窓が画像の外に取る余白 (左, 上, 右, 下)。枠 1 と影 (下へ 12、ぼかし 34) が入る大きさ。
pub fn pin_margins(s: f32) -> (i32, i32, i32, i32) {
    let frame = stroke(1.0, s);
    let blur = px(34.0, s);
    let down = px(12.0, s);
    (
        frame + blur,
        frame + (blur - down).max(0),
        frame + blur,
        frame + blur + down,
    )
}

/// 画像を画面の `image` (物理 px) に置くときの窓の矩形。DPI が変わっても `image` は変えない
/// (`WM_DPICHANGED` の推奨矩形は使わない): 画像は物理 px の 1:1 のまま、余白だけが s に従う。
pub fn pin_window_rect(image: RECT, s: f32) -> RECT {
    let (l, t, r, b) = pin_margins(s);
    RECT {
        left: image.left - l,
        top: image.top - t,
        right: image.right + r,
        bottom: image.bottom + b,
    }
}

/// ピンの窓の中の画像の矩形 (窓の左上から)。`img` は画面の画像の矩形。
pub fn local_img(img: RECT, s: f32) -> RECT {
    let (ml, mt, _, _) = pin_margins(s);
    RECT {
        left: ml,
        top: mt,
        right: ml + img.right - img.left,
        bottom: mt + img.bottom - img.top,
    }
}

/// 作業領域と 1 px も交わらない矩形 (掴めない・閉じられない) は、左上を作業領域の左上へ寄せる。
pub fn rescue_rect(r: RECT, work: RECT) -> RECT {
    if r.right <= work.left || r.left >= work.right || r.bottom <= work.top || r.top >= work.bottom
    {
        return RECT {
            left: work.left,
            top: work.top,
            right: work.left + r.right - r.left,
            bottom: work.top + r.bottom - r.top,
        };
    }
    r
}

/// ポップアップ (`w` × `h`) をツールバー `bar` の下 4 に、左端を `x` に揃えて置く。下に入らなければ上 4。
/// 右に入らなければ作業領域に入るまで左へ寄せる。
pub fn place_popup(x: i32, bar: RECT, w: i32, h: i32, work: RECT, s: f32) -> RECT {
    let x = x.min(work.right - w).max(work.left);
    let below = bar.bottom + px(4.0, s);
    let y = if below + h <= work.bottom {
        below
    } else {
        bar.top - px(4.0, s) - h
    };
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

/// ツールバーの左上。右端を枠の外形の右端に揃え、外形の上端から上へ `gap`。
/// 上に入らなければ下へ `gap`。どちらにも入らなければ画像の内側の右上 (内側へ `inset`)。
/// `front` は手前のピンの画像。上の側で、画像の上端の行のうちツールバーの真下を `front` が全部覆うと、
/// カーソルを画像からツールバーへ動かせない (途中で手前のピンに替わる)。そのときは下が空いていれば下へ。
#[allow(clippy::too_many_arguments)]
pub fn place_toolbar(
    frame: RECT,
    image: RECT,
    w: i32,
    h: i32,
    gap: i32,
    inset: i32,
    work: RECT,
    front: &[RECT],
) -> POINT {
    // x は作業領域の左右に寄せる (いちばん左のモニタの左端のピンで、左のボタンが画面の外に出ない)。
    let fit = |x: i32| x.min(work.right - w).max(work.left);
    let x = fit(frame.right - w);
    let above = frame.top - gap - h;
    let below = frame.bottom + gap;
    let fits_above = above >= work.top;
    let fits_below = below + h <= work.bottom;
    let open = |row| !row_covered(x.max(image.left), (x + w).min(image.right), row, front);
    if fits_above && (open(image.top) || !(fits_below && open(image.bottom - 1))) {
        return POINT { x, y: above };
    }
    if fits_below {
        return POINT { x, y: below };
    }
    POINT {
        x: fit(image.right - inset - w),
        y: image.top + inset,
    }
}

/// 行 `row` の `l`..`r` を `rects` が切れ目なく覆うか。空の範囲は覆わない。
fn row_covered(l: i32, r: i32, row: i32, rects: &[RECT]) -> bool {
    let mut iv: Vec<(i32, i32)> = rects
        .iter()
        .filter(|c| c.top <= row && row < c.bottom)
        .map(|c| (c.left, c.right))
        .collect();
    iv.sort_unstable();
    let mut at = l;
    for (a, b) in iv {
        if a > at {
            break;
        }
        at = at.max(b);
    }
    l < r && at >= r
}

/// ラベルを置く側。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    /// 対象の左上の外 → 入らなければ内側の左上 (窓のラベル)。
    Above,
    /// 左上の外 → 左下の外 → 内側の左上 (UI 要素のラベル)。
    AboveThenBelow,
    /// 左下の外 → 内側の左下 (寸法ラベル)。
    Below,
}

/// ラベル (高さ `h`) の左上。`outer` はハイライトの外形、`target` は対象の矩形、`work` はモニタ。
pub fn place_label(outer: RECT, target: RECT, h: i32, gap: i32, side: Side, work: RECT) -> POINT {
    let above = POINT {
        x: outer.left,
        y: outer.top - gap - h,
    };
    let below = POINT {
        x: outer.left,
        y: outer.bottom + gap,
    };
    let fits = |p: POINT| p.y >= work.top && p.y + h <= work.bottom;
    let candidates: &[POINT] = match side {
        Side::Above => &[above],
        Side::AboveThenBelow => &[above, below],
        Side::Below => &[below],
    };
    for &p in candidates {
        if fits(p) {
            return POINT {
                x: p.x.max(work.left),
                y: p.y,
            };
        }
    }
    match side {
        Side::Below => POINT {
            x: target.left + gap,
            y: target.bottom - gap - h,
        },
        _ => POINT {
            x: target.left + gap,
            y: target.top + gap,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(l: i32, t: i32, rr: i32, b: i32) -> RECT {
        RECT {
            left: l,
            top: t,
            right: rr,
            bottom: b,
        }
    }

    #[test]
    fn snap_threshold() {
        let edges = [
            Edge {
                pos: 100,
                from: 0,
                to: 500,
            },
            Edge {
                pos: 130,
                from: 0,
                to: 500,
            },
        ];
        let th = crate::ui::px(8.0, 1.0);
        // しきい値の内 (8 ちょうどを含む) は付き、外 (9) は付かない。
        assert_eq!(snap(108, 50, &edges, th).map(|e| e.pos), Some(100));
        assert_eq!(snap(92, 50, &edges, th).map(|e| e.pos), Some(100));
        assert_eq!(snap(109, 50, &edges, th), None);
        // 近い方へ付く。
        assert_eq!(snap(124, 50, &edges, th).map(|e| e.pos), Some(130));
        // 辺の範囲から外れた所 (along が 500 + 8 より先) では付かない。
        assert_eq!(snap(100, 509, &edges, th), None);
        assert_eq!(snap(100, 508, &edges, th).map(|e| e.pos), Some(100));
        // 150% ではしきい値が 12 になる。
        let th15 = crate::ui::px(8.0, 1.5);
        assert_eq!(snap(112, 50, &edges, th15).map(|e| e.pos), Some(100));
        assert_eq!(snap(113, 50, &edges, th15), None);
        assert_eq!(snap(87, 50, &edges, th15), None);
    }

    #[test]
    fn front_edges_skip_hidden() {
        // 手前の窓 A (100..300) が奥の窓 B (200..400) の左辺 x=200 を覆う。
        let a = r(100, 100, 300, 300);
        let b = r(200, 50, 400, 350);
        let xs = |p| {
            let (v, _) = front_edges(&[a, b], p);
            v.iter().map(|e| e.pos).collect::<Vec<_>>()
        };
        // A の中では B の左辺は隠れている。B の右辺 x=400 は見えている。
        assert_eq!(xs(POINT { x: 205, y: 200 }), [100, 300, 400]);
        // A の下 (y=330) では B の左辺が見えている。
        assert_eq!(xs(POINT { x: 205, y: 330 }), [100, 300, 200, 400]);
        // 辺どうしが重なるだけなら隠れない。
        let (v, _) = front_edges(&[a, r(300, 100, 500, 300)], POINT { x: 300, y: 200 });
        assert_eq!(v.iter().filter(|e| e.pos == 300).count(), 2);
    }

    #[test]
    fn pin_keeps_image_size_across_dpi() {
        let image = r(500, 400, 500 + 368, 400 + 136);
        for s in [1.0, 1.25, 1.5, 2.0] {
            let w = pin_window_rect(image, s);
            let (l, t, rr, b) = pin_margins(s);
            assert_eq!((w.right - rr) - (w.left + l), 368, "s={s}");
            assert_eq!((w.bottom - b) - (w.top + t), 136, "s={s}");
            assert_eq!((w.left + l, w.top + t), (500, 400), "s={s}");
        }
        // 余白は s に従う (100%: 枠 1 + ぼかし 34、下はさらに 12)。
        assert_eq!(pin_margins(1.0), (35, 23, 35, 47));
        assert_eq!(pin_margins(1.5), (53, 35, 53, 71));
    }

    #[test]
    fn toolbar_three_branches() {
        let work = r(0, 0, 1920, 1040);
        let (w, h) = (289, 40);
        // 上に入る: 右端を枠に揃えて上へ 8。
        let image = r(600, 300, 1000, 600);
        let frame = r(599, 299, 1001, 601);
        assert_eq!(
            place_toolbar(frame, image, w, h, 8, 8, work, &[]),
            POINT {
                x: 1001 - 289,
                y: 299 - 8 - 40
            }
        );
        // 上に入らない → 下へ 8。
        let image = r(600, 20, 1000, 600);
        let frame = r(599, 19, 1001, 601);
        assert_eq!(
            place_toolbar(frame, image, w, h, 8, 8, work, &[]),
            POINT { x: 712, y: 601 + 8 }
        );
        // どちらにも入らない → 画像の内側の右上へ 8。
        let image = r(600, 20, 1000, 1020);
        let frame = r(599, 19, 1001, 1021);
        assert_eq!(
            place_toolbar(frame, image, w, h, 8, 8, work, &[]),
            POINT {
                x: 1000 - 8 - 289,
                y: 28
            }
        );
        // 境目: 上端ちょうど (y = work.top) は入る。
        let frame = r(599, 48, 1001, 601);
        assert_eq!(place_toolbar(frame, image, w, h, 8, 8, work, &[]).y, 0);
        // 左端のピン (frame.right - w < work.left): x は作業領域の左端に寄る。上・下・内側のどれでも。
        let work = r(-1920, 0, 0, 1040);
        for (image, y) in [
            (r(-1910, 300, -1700, 600), 299 - 8 - 40),
            (r(-1910, 20, -1700, 600), 601 + 8),
            (r(-1910, 20, -1700, 1020), 28),
        ] {
            let frame = r(
                image.left - 1,
                image.top - 1,
                image.right + 1,
                image.bottom + 1,
            );
            assert_eq!(
                place_toolbar(frame, image, w, h, 8, 8, work, &[]),
                POINT { x: -1920, y }
            );
        }
    }

    /// 手前のピンが画像の上端のうちツールバーの真下を全部覆うと、下へ出す。
    #[test]
    fn toolbar_goes_below_when_front_pin_covers_top() {
        let work = r(0, 0, 1920, 1040);
        let (w, h) = (289, 40);
        let image = r(600, 300, 1000, 600);
        let frame = r(599, 299, 1001, 601);
        let at = |front: &[RECT]| place_toolbar(frame, image, w, h, 8, 8, work, front).y;
        let above = 299 - 8 - 40;
        let below = 601 + 8;
        // ツールバーの真下 (x 712..1000) を 2 枚で切れ目なく覆う → 下。
        assert_eq!(at(&[r(500, 100, 800, 350), r(800, 200, 1100, 320)]), below);
        // 1 px でも空く → 上のまま。
        assert_eq!(at(&[r(500, 100, 800, 350), r(801, 200, 1100, 320)]), above);
        // 上端の行に届かない手前のピン → 上のまま。
        assert_eq!(at(&[r(500, 100, 1100, 300)]), above);
        // 下も塞がる → 上のまま。
        assert_eq!(at(&[r(500, 100, 1100, 350), r(500, 590, 1100, 700)]), above);
    }

    /// ツールバーはどの置き方でもピンの窓の矩形に重なるか接する。だから「画像 ∪ 影の余白 ∪ ツールバー」に
    /// 切れ目が無く、`pin::hud_tick` は窓の矩形で見れば隙間で HUD を隠さない。
    #[test]
    fn toolbar_touches_pin_window() {
        let work = r(0, 0, 1920, 1040);
        // (画像, 期待する置き方): 上に入る / 上に入らず下 / どちらも入らず内側。
        let cases = [
            (r(600, 300, 1000, 600), "above"),
            (r(600, 20, 1000, 600), "below"),
            (r(600, 20, 1000, 1020), "inside"),
        ];
        for s in [1.0, 1.5] {
            let (w, h) = (px(289.0, s), px(40.0, s));
            let (gap, inset) = (px(8.0, s), px(8.0, s));
            let fw = stroke(1.0, s);
            for (image, want) in cases {
                let frame = r(
                    image.left - fw,
                    image.top - fw,
                    image.right + fw,
                    image.bottom + fw,
                );
                let at = place_toolbar(frame, image, w, h, gap, inset, work, &[]);
                let tb = r(at.x, at.y, at.x + w, at.y + h);
                let got = if tb.bottom <= image.top {
                    "above"
                } else if tb.top >= image.bottom {
                    "below"
                } else {
                    "inside"
                };
                assert_eq!(got, want, "s={s}");
                let win = pin_window_rect(image, s);
                let touches = tb.left <= win.right
                    && tb.right >= win.left
                    && tb.top <= win.bottom
                    && tb.bottom >= win.top;
                assert!(touches, "s={s} {want}: toolbar {tb:?} window {win:?}");
            }
        }
    }

    #[test]
    fn label_sides() {
        let work = r(0, 0, 1920, 1080);
        let target = r(100, 100, 400, 300);
        let outer = r(97, 97, 403, 303);
        assert_eq!(
            place_label(outer, target, 24, 8, Side::Above, work),
            POINT { x: 97, y: 65 }
        );
        assert_eq!(
            place_label(outer, target, 24, 8, Side::Below, work),
            POINT { x: 97, y: 311 }
        );
        let top = r(100, 10, 400, 300);
        let top_outer = r(97, 7, 403, 303);
        assert_eq!(
            place_label(top_outer, top, 24, 8, Side::Above, work),
            POINT { x: 108, y: 18 }
        );
        assert_eq!(
            place_label(top_outer, top, 24, 8, Side::AboveThenBelow, work),
            POINT { x: 97, y: 311 }
        );
        let full = r(0, 0, 1920, 1080);
        assert_eq!(
            place_label(full, full, 24, 8, Side::Below, work),
            POINT {
                x: 8,
                y: 1080 - 8 - 24
            }
        );
    }
}
