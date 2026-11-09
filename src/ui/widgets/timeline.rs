//! タイムライン (DS「Odile」の Timeline): GIF ピンの画像の下端の全体バーの帯と、トレイのフィルムストリップ。
//! 寸法は 100% の px を `px` で物理 px にする。画像の上の帯はテーマに関係なくダークの値で描き、
//! GIF ピンの注釈モードでトレイに付けた帯はトレイの色 (`pal`) で描く。
//! フレームの添字は 0 始まり、画面の番号は 1 始まり。

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

use super::{BtnState, draw_icon, stroke_inside};
use crate::timeline::Timeline;
use crate::ui::{self, DARK, Palette, Style, draw_text, fill_rect, fill_round, px, rectf, stroke};

// ---- 全体バーの帯 ----

/// 帯の中の位置 (帯の左上から、物理 px)。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandGeo {
    pub h: i32,
    /// 再生のボタン 28 × 28 (左上から (10, 8))。
    pub play: RECT,
    /// 全体バー: 左から 176、幅は帯の幅 − 188。上の段 (シーク) と下の段 (スクロールバー) に分かれる。
    pub bar_x: i32,
    pub bar_w: i32,
    /// 上の段 (シーク) の中心: 帯の上から 15。
    pub seek_cy: i32,
    /// 下の段 (ストリップのスクロールバー) の中心: 帯の上から 32。
    pub rail_cy: i32,
    /// 段の境: 帯の上から 24 より上が上の段、24 以上が下の段。
    pub split: i32,
}

pub fn band_geo(band_w: i32, s: f32) -> BandGeo {
    let play = px(28.0, s);
    let (px10, px8) = (px(10.0, s), px(8.0, s));
    BandGeo {
        h: px(44.0, s),
        play: RECT {
            left: px10,
            top: px8,
            right: px10 + play,
            bottom: px8 + play,
        },
        bar_x: px(176.0, s),
        bar_w: (band_w - px(188.0, s)).max(0),
        seek_cy: px(15.0, s),
        rail_cy: px(32.0, s),
        split: px(24.0, s),
    }
}

/// フレーム i の全体バーの上の x (帯の左から)。
pub fn frame_x(i: usize, n: usize, g: &BandGeo) -> f32 {
    let t = if n > 1 {
        i.min(n - 1) as f32 / (n - 1) as f32
    } else {
        0.0
    };
    g.bar_x as f32 + (t * g.bar_w as f32).round()
}

/// 端数のフレームの x (帯の左から。丸めない: 遷移が 1 px ずつ跳ねない)。
pub fn frame_xf(f: f32, n: usize, g: &BandGeo) -> f32 {
    let t = if n > 1 {
        (f / (n - 1) as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    g.bar_x as f32 + t * g.bar_w as f32
}

/// 区間 [l, r] を最小の幅 m に広げる (中心を保つ)。
fn widen(l: f32, r: f32, m: f32) -> (f32, f32) {
    if r - l >= m {
        (l, r)
    } else {
        let c = (l + r) / 2.0;
        (c - m / 2.0, c + m / 2.0)
    }
}

/// 下の段のつまみの左右 (帯の左から): 見えている最初のフレームの x − 2 から最後の x + 2。幅は 24 以上。
/// 描画と当たりで同じ値を使う。
pub fn thumb_x(visible: (f32, f32), n: usize, g: &BandGeo, s: f32) -> (f32, f32) {
    let pad = px(2.0, s) as f32;
    widen(
        frame_xf(visible.0, n, g) - pad,
        frame_xf(visible.1, n, g) + pad,
        px(24.0, s) as f32,
    )
}

/// 帯の左からの x にいちばん近いフレーム。
pub fn frame_at(x: i32, n: usize, g: &BandGeo) -> usize {
    if n <= 1 || g.bar_w == 0 {
        return 0;
    }
    let t = ((x - g.bar_x) as f32 / g.bar_w as f32).clamp(0.0, 1.0);
    (t * (n - 1) as f32).round() as usize
}

/// 削除した区間 (連続した添字の両端)。
pub fn deleted_runs(tl: &Timeline) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for &i in &tl.deleted {
        match runs.last_mut() {
            Some(r) if r.1 + 1 == i => r.1 = i,
            _ => runs.push((i, i)),
        }
    }
    runs
}

/// 帯 (x, y が帯の左上、幅 w)。`visible` はフィルムストリップに見えている最初と最後のフレーム
/// (遷移の途中は端数)。`head` は再生位置のフレーム (遷移の途中は端数)。
/// `tray` はトレイに付けた帯 (GIF ピンの注釈モード): トレイの色で、地を塗らない。
/// `span` は選んだ注釈の区間 (白と黒の破線で囲む)。`rail_hot` は下の段にホバー中かドラッグ中。
#[allow(clippy::too_many_arguments)]
pub fn draw_band(
    ctx: &ID2D1DeviceContext,
    x: f32,
    y: f32,
    w: i32,
    tl: &Timeline,
    playing: bool,
    play_state: BtnState,
    visible: Option<(f32, f32)>,
    rail_hot: bool,
    head: f32,
    sel: Option<(f32, f32)>,
    span: Option<(usize, usize)>,
    tray: Option<&Palette>,
    s: f32,
) {
    let pal = tray.unwrap_or(&DARK);
    let g = band_geo(w, s);
    let n = tl.len();
    if tray.is_none() {
        fill_rect(
            ctx,
            rectf(x, y, x + w as f32, y + g.h as f32),
            pal.surface_veil,
        );
    }
    // 再生のボタン (play / pause のアイコン 14)。
    let pr = rectf(
        x + g.play.left as f32,
        y + g.play.top as f32,
        x + g.play.right as f32,
        y + g.play.bottom as f32,
    );
    let back = match play_state {
        BtnState::Hover => Some(pal.fill_hover),
        BtnState::Pressed => Some(pal.fill_pressed),
        _ => None,
    };
    if let Some(c) = back {
        fill_round(ctx, pr, px(4.0, s) as f32, c);
    }
    let icon = px(14.0, s) as f32;
    let off = ((pr.right - pr.left - icon) / 2.0).round();
    draw_icon(
        ctx,
        if playing { "pause" } else { "play" },
        pr.left + off,
        pr.top + off,
        icon,
        pal.text_primary,
    );
    // 時間: 今 / 全体。
    let now = ui::text(
        &crate::timeline::clock_cs(tl.ts[tl.cur].saturating_sub(tl.ts[0])),
        Style::NumStrong,
        s,
    );
    let all = ui::text(
        &format!(" / {}", crate::timeline::clock_cs(tl.total())),
        Style::Num,
        s,
    );
    let ty = y + ((g.h as f32 - now.h) / 2.0).round();
    let tx = x + px(44.0, s) as f32;
    draw_text(ctx, &now, tx, ty, pal.text_primary);
    draw_text(ctx, &all, tx + now.w, ty, pal.text_secondary);
    if g.bar_w < px(8.0, s) {
        return;
    }
    let fxf = |f: f32| x + frame_xf(f, n, &g);
    let bar_l = x + g.bar_x as f32;
    let bar_r = bar_l + g.bar_w as f32;
    let lw = stroke(2.0, s) as f32;
    // 縦の中心 cy、高さ h の横棒。
    let bar = |l: f32, r: f32, cy: f32, h: f32| rectf(l, cy - h / 2.0, r, cy + h / 2.0);
    let h4 = px(4.0, s) as f32;

    // ---- 下の段 (中心 32): ストリップのスクロールバー。レール 4 とつまみ。ホバー中とドラッグ中はつまみが 6 に太る。
    let cr = y + g.rail_cy as f32;
    let rail = if rail_hot { pal.track } else { pal.fill_hover };
    fill_round(ctx, bar(bar_l, bar_r, cr, h4), h4 / 2.0, rail);
    if let Some(v) = visible {
        let (l, r) = thumb_x(v, n, &g, s);
        let th = if rail_hot { px(6.0, s) as f32 } else { h4 };
        let c = if rail_hot {
            pal.on_fill
        } else {
            pal.border_strong
        };
        fill_round(ctx, bar(x + l, x + r, cr, th), th / 2.0, c);
    }

    // ---- 上の段 (中心 15): シーク。溝はトリムの内側で削除していない所だけ。トリムの外は高さ 2 の線。
    let cs = y + g.seek_cy as f32;
    let (a, b) = (fxf(tl.trim.0 as f32), fxf(tl.trim.1 as f32));
    let h2 = px(2.0, s) as f32;
    if a > bar_l {
        fill_round(ctx, bar(bar_l, a, cs, h2), h2 / 2.0, pal.track);
    }
    if b < bar_r {
        fill_round(ctx, bar(b, bar_r, cs, h2), h2 / 2.0, pal.track);
    }
    // 削除した区間はフレームの間隔の半分 (4 まで) ずつ外へ広げる (1 枚だけ消しても幅が出る)。幅は 6 以上。
    let step = if n > 1 {
        g.bar_w as f32 / (n - 1) as f32
    } else {
        0.0
    };
    let half = (step / 2.0).min(px(4.0, s) as f32);
    let gaps: Vec<(f32, f32)> = deleted_runs(tl)
        .into_iter()
        .map(|(d0, d1)| {
            widen(
                fxf(d0 as f32) - half,
                fxf(d1 as f32) + half,
                px(6.0, s) as f32,
            )
        })
        .collect();
    let mut kept = a;
    for &(l, r) in &gaps {
        if l > kept {
            fill_round(ctx, bar(kept, l.min(b), cs, h4), h4 / 2.0, pal.track);
        }
        kept = kept.max(r);
    }
    if kept < b {
        fill_round(ctx, bar(kept, b, cs, h4), h4 / 2.0, pal.track);
    }
    let tick = px(12.0, s) as f32;
    for &(l, r) in &gaps {
        fill_rect(ctx, bar(l, r, cs, h4), pal.danger_wash);
        for e in [l, r] {
            fill_round(
                ctx,
                rectf(e - lw / 2.0, cs - tick / 2.0, e + lw / 2.0, cs + tick / 2.0),
                lw / 2.0,
                pal.danger,
            );
        }
    }
    // 選んだ範囲: 高さ 8、角丸 4、幅は 8 以上。
    let h8 = px(8.0, s) as f32;
    if let Some((sa, sb)) = sel {
        let (l, r) = widen(fxf(sa), fxf(sb), h8);
        fill_round(ctx, bar(l, r, cs, h8), h8 / 2.0, pal.selection);
    }
    // 選んだ注釈の区間: 上の段に高さ 10 の破線の箱。幅は 8 以上。
    if let Some((sa, sb)) = span {
        let h10 = px(10.0, s) as f32;
        let (l, r) = widen(fxf(sa as f32), fxf(sb as f32), h8);
        dash_box(ctx, bar(l, r, cs, h10), h10 / 2.0, pal, s);
    }
    // トリムの括弧 5 × 16 (中心 15)、線 2。
    let bh = px(16.0, s) as f32;
    bracket(ctx, a - lw / 2.0, cs - bh / 2.0, bh, true, pal, s);
    bracket(ctx, b + lw / 2.0, cs - bh / 2.0, bh, false, pal, s);
    // 再生位置: 線 2 × 20 と影 (shadow-playhead 0 0 4 rgba(0,0,0,.6))、その上に直径 8 の丸 (shadow-knob 0 1 4 .45)。
    let p = fxf(head);
    let h10 = px(10.0, s) as f32;
    let line = rectf(p - lw / 2.0, cs - h10, p + lw / 2.0, cs + h10);
    ui::shadow(
        ctx,
        2.0 * s,
        ui::rgba(0, 0, 0, 0.6),
        (0.0, 0.0),
        None,
        |c| {
            fill_rect(c, line, ui::HL_DARK);
        },
    );
    fill_round(ctx, line, lw / 2.0, pal.on_fill);
    let kr = px(8.0, s) as f32 / 2.0;
    let knob = |c: &ID2D1DeviceContext, color: D2D1_COLOR_F| unsafe {
        let Ok(brush) = c.CreateSolidColorBrush(&color, None) else {
            return;
        };
        c.FillEllipse(
            &D2D1_ELLIPSE {
                point: windows_numerics::Vector2 { X: p, Y: cs },
                radiusX: kr,
                radiusY: kr,
            },
            &brush,
        );
    };
    ui::shadow(
        ctx,
        2.0 * s,
        ui::rgba(0, 0, 0, 0.45),
        (0.0, s.round()),
        None,
        |c| knob(c, ui::HL_DARK),
    );
    knob(ctx, pal.on_fill);
}

/// トリムの括弧: 縦の線と上下の爪 (外形 5 × h、線 2)。`x` は線の外側の辺、`left` は始めの括弧 (爪が右)。
fn bracket(ctx: &ID2D1DeviceContext, x: f32, y: f32, h: f32, left: bool, pal: &Palette, s: f32) {
    let lw = stroke(2.0, s) as f32;
    let w = px(5.0, s) as f32;
    let (l, r) = if left {
        (x - lw, x - lw + w)
    } else {
        (x + lw - w, x + lw)
    };
    let c = pal.on_fill;
    let rad = lw / 2.0;
    if left {
        fill_round(ctx, rectf(l, y, l + lw, y + h), rad, c);
    } else {
        fill_round(ctx, rectf(r - lw, y, r, y + h), rad, c);
    }
    fill_round(ctx, rectf(l, y, r, y + lw), rad, c);
    fill_round(ctx, rectf(l, y + h - lw, r, y + h), rad, c);
}

/// 全体バーのトリムの括弧の当たり: 始め (true) か終わり (false)。括弧の線から左右 5 以内。
pub fn bracket_hit(x: i32, tl: &Timeline, g: &BandGeo, s: f32) -> Option<bool> {
    let n = tl.len();
    let tol = px(5.0, s) as f32;
    let (a, b) = (frame_x(tl.trim.0, n, g), frame_x(tl.trim.1, n, g));
    let (da, db) = ((x as f32 - a).abs(), (x as f32 - b).abs());
    if da <= tol && da <= db {
        Some(true)
    } else if db <= tol {
        Some(false)
    } else {
        None
    }
}

// ---- フィルムストリップ ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Thumb(usize),
    /// 削除した区間 (始め, 枚数) を 1 つのチップにまとめる。
    Gap(usize, usize),
}

/// ストリップの中身の項目。`x` は中身の左端から (物理 px)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StripItem {
    pub kind: Kind,
    pub x: i32,
    pub w: i32,
}

/// サムネイル 56 × 32、削除したチップ 26 × 32、間 4。
pub fn strip_items(tl: &Timeline, s: f32) -> Vec<StripItem> {
    let (tw, gw, gap) = (px(56.0, s), px(26.0, s), px(4.0, s));
    let mut out = Vec::new();
    // 端にも 4 空ける: 選択の囲みとトリムの括弧はサムネイルの外へ 4 出るので、0 からだと窓で切れる。
    let mut x = gap;
    let mut i = 0;
    while i < tl.len() {
        let (kind, w) = if tl.deleted.contains(&i) {
            let start = i;
            while i < tl.len() && tl.deleted.contains(&i) {
                i += 1;
            }
            (Kind::Gap(start, i - start), gw)
        } else {
            i += 1;
            (Kind::Thumb(i - 1), tw)
        };
        out.push(StripItem { kind, x, w });
        x += w + gap;
    }
    out
}

pub fn strip_width(items: &[StripItem]) -> i32 {
    // 右端にも左端 (最初の x) と同じ余白を足す。
    items.last().map_or(0, |it| it.x + it.w + items[0].x)
}

/// 窓 (幅 `view_w`) に見えている最初と最後のフレーム。
pub fn visible_frames(items: &[StripItem], scroll: i32, view_w: i32) -> Option<(usize, usize)> {
    let mut seen = items
        .iter()
        .filter(|it| it.x + it.w > scroll && it.x < scroll + view_w)
        .filter_map(|it| match it.kind {
            Kind::Thumb(i) => Some(i),
            Kind::Gap(..) => None,
        });
    let first = seen.next()?;
    Some((first, seen.next_back().unwrap_or(first)))
}

/// 窓の左からの x にあるサムネイルのフレーム。
pub fn strip_hit(items: &[StripItem], scroll: i32, x: i32) -> Option<usize> {
    items
        .iter()
        .find(|it| x + scroll >= it.x && x + scroll < it.x + it.w)
        .and_then(|it| match it.kind {
            Kind::Thumb(i) => Some(i),
            Kind::Gap(..) => None,
        })
}

/// フレーム i のサムネイルが窓に収まるスクロール量。収まっていれば `scroll` のまま、外なら中央に寄せる。
/// `left` (再生中) は中央でなく左端 (端の余白を残す) に寄せる: 次のコマが右に並び、窓の幅ぶん跳ばずに進む。
pub fn follow(items: &[StripItem], scroll: i32, view_w: i32, i: usize, left: bool) -> i32 {
    let Some(it) = items.iter().find(|it| it.kind == Kind::Thumb(i)) else {
        return scroll;
    };
    let max = (strip_width(items) - view_w).max(0);
    if it.x >= scroll && it.x + it.w <= scroll + view_w {
        return scroll.clamp(0, max);
    }
    let to = if left {
        it.x - items[0].x
    } else {
        it.x + it.w / 2 - view_w / 2
    };
    to.clamp(0, max)
}

/// フレーム i の項目 (削除したフレームはそれを含む `Gap`) を窓の中央に置くスクロール量。端では収まる所で止まる。
/// 帯の下の段 (レールのクリックとつまみのドラッグ) が使う。`follow` と違い、見えていても中央に寄せる。
pub fn center_scroll(items: &[StripItem], view_w: i32, i: usize) -> Option<i32> {
    let it = items.iter().find(|it| match it.kind {
        Kind::Thumb(f) => f == i,
        Kind::Gap(start, len) => (start..start + len).contains(&i),
    })?;
    let max = (strip_width(items) - view_w).max(0);
    Some((it.x + it.w / 2 - view_w / 2).clamp(0, max))
}

/// 選んだ注釈の区間の箱: `select-wash` の地、外形の内側 2 に `hl-dark` の線と、その上に `hl-core` の破線
/// (4 描いて 4 空ける)。
pub fn dash_box(ctx: &ID2D1DeviceContext, r: D2D_RECT_F, radius: f32, pal: &Palette, s: f32) {
    fill_round(ctx, r, radius, pal.select_wash);
    let lw = stroke(2.0, s) as f32;
    let h = lw / 2.0;
    let rr = D2D1_ROUNDED_RECT {
        rect: rectf(r.left + h, r.top + h, r.right - h, r.bottom - h),
        radiusX: (radius - h).max(0.0),
        radiusY: (radius - h).max(0.0),
    };
    unsafe {
        let dash = px(4.0, s) as f32 / lw;
        let (Ok(style), Ok(dark), Ok(core)) = (
            ui::gfx().factory.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES1 {
                    dashStyle: D2D1_DASH_STYLE_CUSTOM,
                    ..Default::default()
                },
                Some(&[dash, dash]),
            ),
            ctx.CreateSolidColorBrush(&ui::HL_DARK, None),
            ctx.CreateSolidColorBrush(&ui::HL_CORE, None),
        ) else {
            return;
        };
        ctx.DrawRoundedRectangle(&rr, &dark, lw, None);
        ctx.DrawRoundedRectangle(&rr, &core, lw, &style);
    }
}

/// ストリップ (`view` は窓の矩形、トレイの左上から)。`thumb(i)` はフレーム i のサムネイルの bitmap。
/// `span` は選んだ注釈の区間: 範囲選択の囲みの代わりに破線の箱 (`dash_box`) で描き、端の番号も区間の端に出す。
#[allow(clippy::too_many_arguments)]
pub fn draw_strip(
    ctx: &ID2D1DeviceContext,
    view: D2D_RECT_F,
    items: &[StripItem],
    scroll: i32,
    tl: &Timeline,
    span: Option<(usize, usize)>,
    thumb: &dyn Fn(usize) -> Option<ID2D1Bitmap1>,
    pal: &Palette,
    s: f32,
) {
    let y = view.top + px(6.0, s) as f32;
    let th = px(32.0, s) as f32;
    let r4 = px(4.0, s) as f32;
    let at = |it: &StripItem| view.left + (it.x - scroll) as f32;
    let full_trim = tl.trim == (0, tl.len() - 1);
    unsafe {
        ctx.PushAxisAlignedClip(&view, D2D1_ANTIALIAS_MODE_ALIASED);
    }
    // 選んだ範囲の囲み (サムネイルの下): 外へ 4、内側 1.5 の線と薄い塗り。
    // 角丸は サムネイル 4 + 外へ 4 = 8: 今のコマの輪 (4 + 2) と同心にして、二重の枠の角をそろえる。
    let ends = span.or(tl.sel);
    if let Some((a, b)) = ends {
        let xs: Vec<f32> = items
            .iter()
            .filter(|it| matches!(it.kind, Kind::Thumb(i) if i >= a && i <= b))
            .flat_map(|it| [at(it), at(it) + it.w as f32])
            .collect();
        if let (Some(l), Some(r)) = (
            xs.iter().copied().reduce(f32::min),
            xs.iter().copied().reduce(f32::max),
        ) {
            let o = px(4.0, s) as f32;
            let ring = rectf(l - o, y - o, r + o, y + th + o);
            if span.is_some() {
                // 区間の箱は高さ 40・角丸 4 (カードの寸法表)。
                dash_box(ctx, ring, r4, pal, s);
            } else {
                fill_round(ctx, ring, r4 + o, pal.select_wash);
                stroke_inside(ctx, ring, r4 + o, 1.5 * s, pal.select_ring);
            }
        }
    }
    let visible = |it: &StripItem| at(it) + (it.w as f32) > view.left && at(it) < view.right;
    for it in items.iter().filter(|it| visible(it)) {
        let x = at(it);
        let r = rectf(x, y, x + it.w as f32, y + th);
        match it.kind {
            Kind::Gap(_, len) => {
                fill_round(ctx, r, r4, pal.danger_wash);
                stroke_inside(ctx, r, r4, stroke(1.0, s) as f32, pal.danger);
                let t = ui::text(&format!("\u{2212}{len}"), Style::NumSmallStrong, s);
                draw_text(
                    ctx,
                    &t,
                    (x + (it.w as f32 - t.w) / 2.0).round(),
                    (y + (th - t.h) / 2.0).round(),
                    pal.text_primary,
                );
            }
            Kind::Thumb(i) => {
                if let Some(bmp) = thumb(i) {
                    fill_bitmap_round(ctx, &bmp, r, r4);
                }
                let out = !tl.in_trim(i);
                if out {
                    fill_round(ctx, r, r4, pal.dim_out);
                }
                let cur = i == tl.cur;
                if cur {
                    let lw = stroke(2.0, s) as f32;
                    let o = rectf(r.left - lw, r.top - lw, r.right + lw, r.bottom + lw);
                    stroke_inside(ctx, o, r4 + lw, lw, pal.on_fill);
                }
                let end = cur
                    || ends.is_some_and(|(a, b)| i == a || i == b)
                    || (!full_trim && (i == tl.trim.0 || i == tl.trim.1));
                if end {
                    frame_number(ctx, r, i, cur, out, pal, s);
                }
                // トリムの括弧 (全体のときは描かない): 始めはサムネイルの左 4、終わりは右 4。
                if !full_trim && (i == tl.trim.0 || i == tl.trim.1) {
                    let lw = stroke(2.0, s) as f32;
                    let (bw, bh) = (px(5.0, s) as f32, px(38.0, s) as f32);
                    let bt = y - px(3.0, s) as f32;
                    let left = i == tl.trim.0;
                    let bx = if left {
                        r.left - px(4.0, s) as f32
                    } else {
                        r.right + px(4.0, s) as f32 - bw
                    };
                    let c = pal.on_fill;
                    let v = if left { bx } else { bx + bw - lw };
                    fill_round(ctx, rectf(v, bt, v + lw, bt + bh), lw / 2.0, c);
                    fill_round(ctx, rectf(bx, bt, bx + bw, bt + lw), lw / 2.0, c);
                    fill_round(ctx, rectf(bx, bt + bh - lw, bx + bw, bt + bh), lw / 2.0, c);
                }
            }
        }
    }
    unsafe {
        ctx.PopAxisAlignedClip();
    }
}

/// フレーム番号: サムネイルの右下の内側 (右と下から 2)、高さ 14、左右 4、`surface-veil`、角丸 2。
fn frame_number(
    ctx: &ID2D1DeviceContext,
    thumb: D2D_RECT_F,
    i: usize,
    cur: bool,
    out: bool,
    pal: &Palette,
    s: f32,
) {
    let style = if cur {
        Style::NumSmallStrong
    } else {
        Style::NumSmall
    };
    let t = ui::text(&(i + 1).to_string(), style, s);
    let (h, pad, inset) = (px(14.0, s) as f32, px(4.0, s) as f32, px(2.0, s) as f32);
    let w = t.w + 2.0 * pad;
    let (r, b) = (thumb.right - inset, thumb.bottom - inset);
    fill_round(
        ctx,
        rectf(r - w, b - h, r, b),
        px(2.0, s) as f32,
        pal.surface_veil,
    );
    let ink = if out && !cur {
        pal.text_disabled
    } else {
        pal.text_primary
    };
    draw_text(ctx, &t, r - w + pad, b - h + ((h - t.h) / 2.0).round(), ink);
}

/// bitmap を角丸の矩形に引き伸ばして塗る。
fn fill_bitmap_round(ctx: &ID2D1DeviceContext, bmp: &ID2D1Bitmap1, r: D2D_RECT_F, radius: f32) {
    unsafe {
        let size = bmp.GetPixelSize();
        let Ok(brush) = ctx.CreateBitmapBrush(bmp, None, None) else {
            return;
        };
        let (sx, sy) = (
            (r.right - r.left) / size.width.max(1) as f32,
            (r.bottom - r.top) / size.height.max(1) as f32,
        );
        brush.SetTransform(
            &(windows_numerics::Matrix3x2::scale(sx, sy)
                * windows_numerics::Matrix3x2::translation(r.left, r.top)),
        );
        ctx.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: r,
                radiusX: radius,
                radiusY: radius,
            },
            &brush,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn band_and_strip_geometry() {
        // DS の見本: 帯の幅 560 → 全体バーは左 176・幅 372。
        let g = band_geo(560, 1.0);
        assert_eq!((g.bar_x, g.bar_w, g.h), (176, 372, 44));
        // 案 C の 2 段: 上の段の中心 15、下の段の中心 32、段の境 24。
        assert_eq!((g.seek_cy, g.rail_cy, g.split), (15, 32, 24));
        let g15 = band_geo(840, 1.5);
        assert_eq!((g15.seek_cy, g15.rail_cy, g15.split), (23, 48, 36));
        assert_eq!(frame_x(0, 1800, &g), 176.0);
        assert_eq!(frame_x(1799, 1800, &g), 548.0);
        assert_eq!(frame_at(548, 1800, &g), 1799);
        assert_eq!(frame_at(0, 1800, &g), 0);
        // 削除した区間は 26 のチップ 1 つにまとまり、後ろのサムネイルが詰まる。
        let mut tl = Timeline::new(
            (0..10).map(|i| Duration::from_millis(i * 33)).collect(),
            Duration::from_millis(330),
        );
        tl.deleted.extend(2..=4);
        let items = strip_items(&tl, 1.0);
        assert_eq!(items.len(), 8);
        // 左端は 4 空ける (選択の囲みが外へ 4 出ても窓で切れない)。
        assert_eq!(items[0].x, 4);
        assert_eq!(items[2].kind, Kind::Gap(2, 3));
        assert_eq!((items[2].x, items[2].w), (124, 26));
        assert_eq!(items[3].kind, Kind::Thumb(5));
        assert_eq!(items[3].x, 154);
        assert_eq!(strip_hit(&items, 0, 155), Some(5));
        assert_eq!(strip_hit(&items, 0, 125), None);
        assert_eq!(deleted_runs(&tl), vec![(2, 4)]);
    }

    #[test]
    fn rail_thumb_and_center_scroll() {
        // 帯 560 → 全体バー 176 + 372。187 フレームならフレームの間隔は 2。
        let g = band_geo(560, 1.0);
        // つまみは見えている範囲の ± 2: フレーム 10 は 196、30 は 236。
        assert_eq!(thumb_x((10.0, 30.0), 187, &g, 1.0), (194.0, 238.0));
        // 幅がちょうど 24 なら広げない。
        assert_eq!(thumb_x((10.0, 20.0), 187, &g, 1.0), (194.0, 218.0));
        // 24 に満たなければ中心 (198) を保って 24 に広げる。
        assert_eq!(thumb_x((10.0, 12.0), 187, &g, 1.0), (186.0, 210.0));
        // ストリップ: 10 フレーム、2〜4 を削除 → 中身の幅 454。窓 120 → スクロールは 0..=334。
        let mut tl = Timeline::new(
            (0..10).map(|i| Duration::from_millis(i * 33)).collect(),
            Duration::from_millis(330),
        );
        tl.deleted.extend(2..=4);
        let items = strip_items(&tl, 1.0);
        // フレーム 5 (x 154、幅 56) の中心 182 を窓の中央 60 に。
        assert_eq!(center_scroll(&items, 120, 5), Some(122));
        // 削除したフレーム 3 は Gap (x 124、幅 26) の中心 137 を中央に。
        assert_eq!(center_scroll(&items, 120, 3), Some(77));
        // 端では収まる所で止まる。
        assert_eq!(center_scroll(&items, 120, 0), Some(0));
        assert_eq!(center_scroll(&items, 120, 9), Some(334));
        assert_eq!(center_scroll(&items, 120, 10), None);
    }
}
