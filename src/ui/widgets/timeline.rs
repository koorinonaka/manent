//! タイムライン (DS「Odile」の Timeline): GIF ピンの画像の下端の全体バーの帯と、トレイのフィルムストリップ。
//! 寸法は 100% の px を `px` で物理 px にする。画像の上の帯はテーマに関係なくダークの値で描き、
//! GIF ピンの注釈モードでトレイに付けた帯はトレイの色 (`pal`) で描く。
//! フレームの添字は 0 始まり、画面の番号は 1 始まり。

use windows::Win32::Foundation::{POINT, RECT};
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
    /// 最初のフレームへのボタン 28 × 28 (左上から (10, 8))。
    pub first: RECT,
    /// 再生のボタン 28 × 28 (左上から (40, 8))。
    pub play: RECT,
    /// 最後のフレームへのボタン 28 × 28 (左上から (70, 8))。
    pub last: RECT,
    /// ループのボタン 28 × 28 (左上から (100, 8))。
    pub looping: RECT,
    /// 全体バー: 左から 250、幅は帯の幅 − 262。上の段 (シーク) と下の段 (スクロールバー) に分かれる。
    pub bar_x: i32,
    pub bar_w: i32,
    /// 上の段 (シーク) の中心: 帯の上から 22 (ボタンの行の中心)。
    pub seek_cy: i32,
    /// 下の段 (ストリップのスクロールバー) の中心: 帯の上から 40 (帯の下端の細い線)。
    pub rail_cy: i32,
    /// 段の境: 帯の上から 36 より上が上の段、36 以上が下の段。
    pub split: i32,
}

pub fn band_geo(band_w: i32, s: f32) -> BandGeo {
    let size = px(28.0, s);
    let btn = |left: f32| {
        crate::ui::rect_at(
            POINT {
                x: px(left, s),
                y: px(8.0, s),
            },
            size,
            size,
        )
    };
    BandGeo {
        h: px(44.0, s),
        first: btn(10.0),
        play: btn(40.0),
        last: btn(70.0),
        looping: btn(100.0),
        bar_x: px(250.0, s),
        bar_w: (band_w - px(262.0, s)).max(0),
        seek_cy: px(22.0, s),
        rail_cy: px(40.0, s),
        split: px(36.0, s),
    }
}

/// 帯のボタン。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BandBtn {
    First,
    Play,
    Last,
    Loop,
}

/// 帯の左上からの `pt` の上のボタン。
pub fn band_btn(g: &BandGeo, pt: POINT) -> Option<BandBtn> {
    let has = |r: &RECT| pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom;
    [
        (&g.first, BandBtn::First),
        (&g.play, BandBtn::Play),
        (&g.last, BandBtn::Last),
        (&g.looping, BandBtn::Loop),
    ]
    .into_iter()
    .find(|(r, _)| has(r))
    .map(|(_, b)| b)
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

/// 差の棒の最大の高さ (100% の px)。下端は帯の上から `DIFF_BOTTOM`。
const DIFF_H: f32 = 8.0;
const DIFF_BOTTOM: f32 = 20.0;

/// 前フレームとの差の棒の列 (全体バーの左から 1 px ずつ): (高さ, トリムの内か)。高さ 0 は描かない。
/// 高さ = ceil(√割合 × `max_h`) (0 でなければ 1 以上)。GIF をまたいで同じ尺度 (画面の 25% が変われば半分)。
/// フレームは再生位置を中心に間隔 (`bar_w` ÷ (枚数 − 1)) の幅を持ち、列はそこに掛かるフレームの最大。
/// 削除したフレームは描かない。1 列にトリムの内と外が掛かれば内の列にする。
pub fn diff_columns(diffs: &[f32], tl: &Timeline, bar_w: i32, max_h: i32) -> Vec<(i32, bool)> {
    let n = tl.len();
    let w = bar_w.max(0) as usize;
    if n < 2 || w == 0 {
        return vec![(0, false); w];
    }
    let (mut cin, mut cout) = (vec![0; w], vec![0; w]);
    let step = bar_w as f32 / (n - 1) as f32;
    for (f, &r) in diffs.iter().enumerate().take(n) {
        if r <= 0.0 || tl.deleted.contains(&f) {
            continue;
        }
        let h = (r.sqrt() * max_h as f32).ceil().max(1.0) as i32;
        let x = f as f32 * step;
        let col = if (tl.trim.0..=tl.trim.1).contains(&f) {
            &mut cin
        } else {
            &mut cout
        };
        let c0 = ((x - step / 2.0).floor().max(0.0) as usize).min(w - 1);
        let c1 = ((x + step / 2.0).ceil() as usize).clamp(c0 + 1, w);
        for c in &mut col[c0..c1] {
            *c = (*c).max(h);
        }
    }
    cin.into_iter()
        .zip(cout)
        .map(|(i, o)| if i > 0 { (i, true) } else { (o, false) })
        .collect()
}

/// 差の棒の色: 1 px で `diff_bar`、`max_h` で `diff_bar_hot`、間は高さで線形に補間する。
fn diff_color(pal: &crate::ui::Palette, h: i32, max_h: i32) -> D2D1_COLOR_F {
    let t = (h - 1) as f32 / (max_h - 1).max(1) as f32;
    super::mix(pal.diff_bar, pal.diff_bar_hot, t)
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
/// `diff` はフレームごとの前フレームとの差の割合 (トレイの差のボタンがオンの間だけ渡す)。
#[allow(clippy::too_many_arguments)]
pub fn draw_band(
    ctx: &ID2D1DeviceContext,
    x: f32,
    y: f32,
    w: i32,
    tl: &Timeline,
    playing: bool,
    looping: bool,
    hover: Option<BandBtn>,
    visible: Option<(f32, f32)>,
    rail_hot: bool,
    head: f32,
    sel: Option<(f32, f32)>,
    span: Option<&[(usize, usize)]>,
    tray: Option<&Palette>,
    diff: Option<&[f32]>,
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
    if hover == Some(BandBtn::Play) {
        fill_round(ctx, pr, px(4.0, s) as f32, pal.fill_hover);
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
    // 最初・最後のフレームへのボタン。
    for (r, icon, b) in [
        (&g.first, "first", BandBtn::First),
        (&g.last, "last", BandBtn::Last),
    ] {
        super::icon_button(
            ctx,
            x + r.left as f32,
            y + r.top as f32,
            28.0,
            14.0,
            icon,
            BtnState::of((false, false, false), hover == Some(b), false).into(),
            pal,
            s,
        );
    }
    // ループのボタン (オンはツールのボタンのオンと同じ地)。
    super::icon_button(
        ctx,
        x + g.looping.left as f32,
        y + g.looping.top as f32,
        28.0,
        14.0,
        "loop",
        BtnState::of((looping, false, false), hover == Some(BandBtn::Loop), false).into(),
        pal,
        s,
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
    // 今の欄は全体の時間の書式で、どの数字でも収まる幅に固定する (tabular figures が効かない書体でも「/ 全体」が揺れない)。
    let tmpl = crate::timeline::clock_cs(tl.total());
    let slot = (b'0'..=b'9')
        .map(|d| {
            let t: String = tmpl
                .chars()
                .map(|c| if c.is_ascii_digit() { d as char } else { c })
                .collect();
            ui::text(&t, Style::NumStrong, s).w
        })
        .fold(now.w, f32::max);
    let ty = y + ((g.h as f32 - now.h) / 2.0).round();
    // ループのボタンの右端 (128) から 14 空ける。幅 90 の時間の右から再生位置の丸 (246) までも 14。
    let tx = x + px(142.0, s) as f32;
    draw_text(ctx, &now, tx, ty, pal.text_primary);
    draw_text(ctx, &all, tx + slot, ty, pal.text_secondary);
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

    // ---- 差の棒: 上の段の溝の上端 (帯の上から 20) から上へ。ほかの何よりも下に描く。
    // 色は text-secondary に不透明度 50%、トリムの外は 25%。同じ高さの列はつないで 1 つの矩形にする。
    if let Some(d) = diff {
        let max_h = px(DIFF_H, s);
        let cols = diff_columns(d, tl, g.bar_w, max_h);
        let base = y + px(DIFF_BOTTOM, s) as f32;
        let mut c = 0;
        while c < cols.len() {
            let (h, inside) = cols[c];
            let mut e = c + 1;
            while e < cols.len() && cols[e] == cols[c] {
                e += 1;
            }
            if h > 0 {
                let a = if inside { 0.9 } else { 0.35 };
                fill_rect(
                    ctx,
                    rectf(bar_l + c as f32, base - h as f32, bar_l + e as f32, base),
                    super::fade(diff_color(pal, h, max_h), a),
                );
            }
            c = e;
        }
    }

    // ---- 下の段 (中心 40): ストリップのスクロールバー。レール 2 とつまみ。ホバー中とドラッグ中はつまみが 4 に太る。
    let cr = y + g.rail_cy as f32;
    let rail = if rail_hot { pal.track } else { pal.fill_hover };
    let h2r = px(2.0, s) as f32;
    fill_round(ctx, bar(bar_l, bar_r, cr, h2r), h2r / 2.0, rail);
    if let Some(v) = visible {
        let (l, r) = thumb_x(v, n, &g, s);
        let th = if rail_hot { h4 } else { h2r };
        let c = if rail_hot {
            pal.on_fill
        } else {
            pal.border_strong
        };
        fill_round(ctx, bar(x + l, x + r, cr, th), th / 2.0, c);
    }

    // ---- 上の段 (中心 22): シーク。溝はトリムの内側で削除していない所だけ。トリムの外は高さ 2 の線。
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
    for &(sa, sb) in span.unwrap_or_default() {
        let h10 = px(10.0, s) as f32;
        let (l, r) = widen(fxf(sa as f32), fxf(sb as f32), h8);
        dash_box(ctx, bar(l, r, cs, h10), h10 / 2.0, pal, s);
    }
    // トリムの括弧 5 × 16 (中心 22)、線 2。
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
    span: Option<&[(usize, usize)]>,
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
    // 区間は飛び飛びのことがある: 範囲ごとに箱を描き、各範囲の端に番号を出す。
    let sel = tl.sel.map(|r| [r]);
    let ends: &[(usize, usize)] = span.or(sel.as_ref().map(|r| &r[..])).unwrap_or_default();
    for &(a, b) in ends {
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
                    || ends.iter().any(|&(a, b)| i == a || i == b)
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
pub(crate) fn fill_bitmap_round(
    ctx: &ID2D1DeviceContext,
    bmp: &ID2D1Bitmap1,
    r: D2D_RECT_F,
    radius: f32,
) {
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

    /// 差の棒の列: 高さは √割合 で 1 px 以上、削除したフレームは描かない、トリムの外は外の列。
    #[test]
    fn diff_columns_follow_ds() {
        let mut tl = Timeline::new(
            (0..5).map(|i| Duration::from_millis(i * 33)).collect(),
            Duration::from_millis(165),
        );
        tl.trim = (0, 2);
        tl.deleted.insert(4);
        // 全体バー 8 px、間隔 2。フレーム 4 (削除) が最大 1.0。
        let cols = diff_columns(&[0.0, 0.0625, 0.25, 0.01, 1.0], &tl, 8, 8);
        let (o, i) = (false, true);
        assert_eq!(
            cols,
            [
                (0, o),
                (2, i),
                (2, i),
                (4, i),
                (4, i),
                (1, o),
                (1, o),
                (0, o)
            ]
        );
        // 全フレーム同じなら棒は無い。
        assert!(diff_columns(&[0.0; 5], &tl, 8, 8).iter().all(|c| c.0 == 0));
        // 高さは GIF の最大に合わせない: 画面の 1% しか変わらない GIF の棒は 1 px のまま。
        assert!(
            diff_columns(&[0.0, 0.01, 0.01, 0.01, 0.01], &tl, 8, 8)
                .iter()
                .all(|c| c.0 <= 1)
        );
    }

    #[test]
    fn band_and_strip_geometry() {
        // 帯の幅 620 → 全体バーは左 250・幅 358。
        let g = band_geo(620, 1.0);
        assert_eq!((g.bar_x, g.bar_w, g.h), (250, 358, 44));
        // 案 C の 2 段: 上の段の中心 22、下の段の中心 40、段の境 36。
        assert_eq!((g.seek_cy, g.rail_cy, g.split), (22, 40, 36));
        let g15 = band_geo(840, 1.5);
        assert_eq!((g15.seek_cy, g15.rail_cy, g15.split), (33, 60, 54));
        assert_eq!(frame_x(0, 1800, &g), 250.0);
        assert_eq!(frame_x(1799, 1800, &g), 608.0);
        assert_eq!(frame_at(608, 1800, &g), 1799);
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
        // 帯 634 → 全体バー 250 + 372。187 フレームならフレームの間隔は 2。
        let g = band_geo(634, 1.0);
        // つまみは見えている範囲の ± 2: フレーム 10 は 270、30 は 310。
        assert_eq!(thumb_x((10.0, 30.0), 187, &g, 1.0), (268.0, 312.0));
        // 幅がちょうど 24 なら広げない。
        assert_eq!(thumb_x((10.0, 20.0), 187, &g, 1.0), (268.0, 292.0));
        // 24 に満たなければ中心 (272) を保って 24 に広げる。
        assert_eq!(thumb_x((10.0, 12.0), 187, &g, 1.0), (260.0, 284.0));
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
