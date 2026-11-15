//! コピー・保存の帯: ピンの画像の右上の内側に重ねる、進捗 (GIF のエンコード) と結果 (完了・警告) の知らせ。
//! GIF ピンと静止画ピンで共通。ピンの窓 (`WDA_EXCLUDEFROMCAPTURE`) の中に描くので、画面の録画とキャプチャに写らず、
//! 書き出す画像にも入らない。入力は持たない (帯の上の押下はピンへそのまま届く)。
//! 寸法と時間は canvas「コピー・保存の帯」(t261109-e6dd) の DIMS と TIMING。100% の px を `px` で物理 px にし、
//! 色はテーマに関係なくダークの値。

use std::cell::RefCell;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

use super::draw_icon;
use crate::ui::anim::{self, Anim};
use crate::ui::{self, DARK, Style, Text, draw_text, fill_round, px, rectf};

/// 完了と警告を残す時間。
const DONE_HOLD: Duration = Duration::from_millis(1500);
const WARN_HOLD: Duration = Duration::from_millis(4000);
/// `Anim` の鍵。
const ALPHA: u8 = 0;
const WIDTH: u8 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Progress,
    Done,
    Warn,
}

pub struct Band {
    pub kind: Kind,
    /// 1 行目。
    pub label: String,
    /// 1 行目の「· サイズ」。ラベルと続けて入るときだけ出す。
    pub size: Option<String>,
    /// 2 行目 (下げた値・超過の説明)。
    pub sub: Option<String>,
    /// 進捗の操作のアイコン (`copy` か `save`)。
    pub op: &'static str,
    /// 進捗: (書いたフレーム数, そのエンコードのフレーム数)。
    pub count: (usize, usize),
}

impl Band {
    pub fn progress(op: &'static str, label: String, done: usize, total: usize) -> Self {
        Self {
            kind: Kind::Progress,
            label,
            size: None,
            sub: None,
            op,
            count: (done, total),
        }
    }

    pub fn result(kind: Kind, label: &str, size: Option<String>, sub: Option<String>) -> Self {
        Self {
            kind,
            label: label.to_string(),
            size,
            sub,
            op: "",
            count: (0, 0),
        }
    }
}

/// ピンが持つ帯の状態。`show` で中身を替え、出ている間は持ち主が刻み (`tick`) を回して描き直す。
#[derive(Default)]
pub struct Status {
    band: Option<Band>,
    /// 消し始める時刻 (完了と警告)。進捗は終わるまで残す。
    until: Option<Instant>,
    fading: bool,
    /// `false` からの変化。持ち主の刻みで描き直す。
    changed: bool,
    /// 不透明度と幅の遷移。描く (`&self`) の中で進めるので RefCell。
    anims: RefCell<Anim<u8>>,
}

impl Status {
    /// 中身を替える。出ていなければ 140 ms で出し、出ていれば幅を 140 ms で寄せる (右端と上端は動かない)。
    pub fn show(&mut self, band: Band) {
        if self.band.is_none() {
            self.anims.get_mut().clear();
        }
        self.until = match band.kind {
            Kind::Progress => None,
            Kind::Done => Some(Instant::now() + DONE_HOLD),
            Kind::Warn => Some(Instant::now() + WARN_HOLD),
        };
        self.fading = false;
        self.band = Some(band);
        self.changed = true;
    }

    /// 進捗の帯の文言と数を替える (焼き直しでは文言が替わり、数は 0 から)。進捗の帯が出ていなければ何もしない。
    pub fn set_progress(&mut self, label: String, done: usize, total: usize) {
        if let Some(b) = self.band.as_mut().filter(|b| b.kind == Kind::Progress)
            && (b.label != label || b.count != (done, total))
        {
            b.label = label;
            b.count = (done, total);
            self.changed = true;
        }
    }

    pub fn active(&self) -> bool {
        self.band.is_some()
    }

    /// 残す時間が過ぎたら 360 ms で薄めて消す。描き直しが要れば真。
    pub fn tick(&mut self) -> bool {
        if self.band.is_none() {
            return false;
        }
        if !self.fading && self.until.is_some_and(|t| Instant::now() >= t) {
            self.fading = true;
            self.changed = true;
        }
        if self.fading {
            let an = self.anims.get_mut();
            let a = an.enter(ALPHA, 0.0, anim::FADE_OUT_MS, anim::motion(), 0.0);
            if a <= 0.0 {
                self.band = None;
                an.clear();
                return true;
            }
        }
        std::mem::take(&mut self.changed) || self.anims.borrow().busy()
    }

    /// `image` は画像の矩形 (窓の中の物理 px)。帯は画像の外には描かない。
    pub fn draw(&self, ctx: &ID2D1DeviceContext, image: D2D_RECT_F, s: f32) {
        let Some(b) = &self.band else { return };
        let pal = &DARK;
        let m = anim::motion();
        let p = |v: f32| px(v, s) as f32;
        let mut an = self.anims.borrow_mut();
        let alpha = if self.fading {
            an.enter(ALPHA, 0.0, anim::FADE_OUT_MS, m, 0.0)
        } else {
            an.enter(ALPHA, 1.0, anim::SHOW_MS, m, 0.0)
        };
        if alpha <= 0.0 {
            return;
        }
        let progress = b.kind == Kind::Progress;
        let label = ui::text(&b.label, Style::Caption, s);
        let size = b
            .size
            .as_ref()
            .map(|t| ui::text(&format!("· {t}"), Style::Num, s));
        let sub = b.sub.as_ref().map(|t| ui::text(t, Style::Caption, s));
        let geo = Geo::new(s);
        let (target, keep) = band_width(
            &geo,
            progress,
            label.w,
            size.as_ref().map_or(0.0, |t| geo.gap + t.w),
            sub.as_ref().map_or(0.0, |t| t.w),
            image.right - image.left,
        );
        let w = an.enter(WIDTH, target, anim::SHOW_MS, m, target);
        drop(an);
        let h = p(if progress || sub.is_some() {
            48.0
        } else {
            32.0
        });
        let (right, top) = (image.right - geo.inset, image.top + geo.inset);
        let r = rectf(right - w, top, right, top + h);
        // 文字は着いた幅で切る (縮む途中は帯の矩形で隠す)。
        let avail = target - geo.text_x - geo.pad_x;
        unsafe {
            ctx.PushAxisAlignedClip(&image, D2D1_ANTIALIAS_MODE_ALIASED);
            if alpha < 1.0 {
                ctx.PushLayer(
                    &D2D1_LAYER_PARAMETERS1 {
                        contentBounds: r,
                        maskTransform: ui::identity(),
                        opacity: alpha,
                        ..Default::default()
                    },
                    None,
                );
            }
            let radius = p(4.0);
            fill_round(ctx, r, radius, pal.surface_veil);
            if b.kind == Kind::Warn {
                fill_round(ctx, r, radius, pal.danger_wash);
            }
            let (icon, ink) = match b.kind {
                Kind::Progress => (b.op, pal.text_secondary),
                Kind::Done => ("check", pal.text_primary),
                Kind::Warn => ("error", pal.danger),
            };
            draw_icon(ctx, icon, r.left + p(12.0), r.top + p(8.0), 16.0 * s, ink);
            ctx.PushAxisAlignedClip(&r, D2D1_ANTIALIAS_MODE_ALIASED);
            let (tx, l1, l2) = (r.left + geo.text_x, r.top + p(8.0), r.top + p(24.0));
            match (keep, size) {
                (true, Some(size)) => {
                    draw_text(ctx, &label, tx, l1, pal.text_primary);
                    draw_text(ctx, &size, tx + label.w + geo.gap, l1, pal.text_primary);
                }
                _ => {
                    let label = fit(label, &b.label, Style::Caption, avail, s);
                    draw_text(ctx, &label, tx, l1, pal.text_primary);
                }
            }
            if progress {
                // 数は切らない。バーは数の左 8 まで、中心は外形の上から 30。
                let (done, total) = b.count;
                let count = ui::text(&format!("{done} / {total}"), Style::Num, s);
                let cx = r.right - geo.pad_x - count.w;
                draw_text(ctx, &count, cx, l2, pal.text_secondary);
                let (x0, x1, cy, bh) = (tx, cx - p(8.0), r.top + p(30.0), p(4.0));
                if x1 > x0 {
                    let groove = rectf(x0, cy - bh / 2.0, x1, cy + bh / 2.0);
                    fill_round(ctx, groove, p(2.0), pal.track);
                    let frac = if total > 0 {
                        (done as f32 / total as f32).min(1.0)
                    } else {
                        0.0
                    };
                    if frac > 0.0 {
                        let fill = rectf(x0, groove.top, x0 + (x1 - x0) * frac, groove.bottom);
                        fill_round(ctx, fill, p(2.0), pal.on_fill);
                    }
                }
            } else if let (Some(sub), Some(t)) = (sub, &b.sub) {
                let sub = fit(sub, t, Style::Caption, avail, s);
                let ink = if b.kind == Kind::Warn {
                    pal.text_primary
                } else {
                    pal.text_secondary
                };
                draw_text(ctx, &sub, tx, l2, ink);
            }
            ctx.PopAxisAlignedClip();
            if alpha < 1.0 {
                ctx.PopLayer();
            }
            ctx.PopAxisAlignedClip();
        }
    }
}

/// 帯の寸法 (物理 px)。
struct Geo {
    /// 画像の端からの距離 8。
    inset: f32,
    /// 文字の左 (外形の左から 36) と右の余白 12。
    text_x: f32,
    pad_x: f32,
    /// ラベルと「· サイズ」の間 4。
    gap: f32,
    /// 進捗の幅の上限 300。
    max_w: f32,
}

impl Geo {
    fn new(s: f32) -> Self {
        let p = |v: f32| px(v, s) as f32;
        Self {
            inset: p(8.0),
            text_x: p(36.0),
            pad_x: p(12.0),
            gap: p(4.0),
            max_w: p(300.0),
        }
    }
}

/// 帯の幅と、1 行目にサイズを出すか。`lw` はラベル、`sw` は間 4 を含めた「· サイズ」、`rw` は 2 行目の文字の幅
/// (無ければ 0)。進捗は min(300, 画像の幅 − 16)。完了と警告は中身の幅 (左 36 + 文字 + 右 12、切り上げ) で、
/// 画像の幅 − 16 で止める。サイズはラベルと続けて入るときだけ出す。
fn band_width(g: &Geo, progress: bool, lw: f32, sw: f32, rw: f32, img_w: f32) -> (f32, bool) {
    let cap = (img_w - 2.0 * g.inset).max(0.0);
    if progress {
        return (g.max_w.min(cap), false);
    }
    let side = g.text_x + g.pad_x;
    let keep = sw > 0.0 && side + lw + sw <= cap;
    let line1 = lw + if keep { sw } else { 0.0 };
    ((side + line1.max(rw)).ceil().min(cap), keep)
}

/// `max` に入らなければ末尾を切って … を付ける。
/// ponytail: 1 文字ずつ削って組み直す (帯の文言は 40 字ほどなので足りる)。長い文に使うなら二分探索にする。
fn fit(full: Text, src: &str, style: Style, max: f32, s: f32) -> Text {
    if full.w <= max {
        return full;
    }
    let chars: Vec<char> = src.chars().collect();
    for n in (1..chars.len()).rev() {
        let cut: String = chars[..n].iter().collect();
        let t = ui::text(&format!("{}…", cut.trim_end()), style, s);
        if t.w <= max {
            return t;
        }
    }
    ui::text("…", style, s)
}

/// 帯のサイズの書き方: 1 MB = 1,000,000 バイト (上限と同じ)、小数 1 桁。0.1 MB 未満は KB の整数。
/// 上限を超えたとき (`over`) は切り上げる (1,040,000 バイトは 1.1 MB で、上限 1 MB より大きく見える)。
pub fn size_text(bytes: usize, over: bool) -> String {
    let div = |v: usize, d: usize| if over { v.div_ceil(d) } else { (v + d / 2) / d };
    if bytes < 100_000 {
        format!("{} KB", div(bytes, 1000))
    } else {
        let tenths = div(bytes, 100_000);
        format!("{}.{} MB", tenths / 10, tenths % 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// canvas の `layoutBand` と同じ規則 (100% の px)。
    #[test]
    fn band_width_follows_canvas_rule() {
        let g = Geo::new(1.0);
        // 進捗は min(300, 画像の幅 − 16)。
        assert_eq!(band_width(&g, true, 0.0, 0.0, 0.0, 560.0), (300.0, false));
        assert_eq!(band_width(&g, true, 0.0, 0.0, 0.0, 160.0), (144.0, false));
        // 完了: 左 36 + ラベル 84 + 「· 0.9 MB」40.5 + 右 12 を切り上げ。
        assert_eq!(band_width(&g, false, 84.0, 40.5, 0.0, 560.0), (173.0, true));
        // 2 行目が長ければ 2 行目の幅。
        assert_eq!(
            band_width(&g, false, 84.0, 40.5, 180.0, 560.0),
            (228.0, true)
        );
        // 入らなければサイズを落とし、画像の幅 − 16 で止める。
        assert_eq!(
            band_width(&g, false, 84.0, 40.5, 0.0, 160.0),
            (132.0, false)
        );
        assert_eq!(
            band_width(&g, false, 84.0, 40.5, 180.0, 160.0),
            (144.0, false)
        );
        // 150%: 寸法は 1.5 倍。
        let g = Geo::new(1.5);
        assert_eq!(band_width(&g, true, 0.0, 0.0, 0.0, 840.0), (450.0, false));
        assert_eq!(
            band_width(&g, false, 126.0, 0.0, 0.0, 840.0),
            (198.0, false)
        );
    }

    #[test]
    fn size_text_rounds_and_rounds_up_when_over() {
        assert_eq!(size_text(86_000, false), "86 KB");
        assert_eq!(size_text(86_400, true), "87 KB");
        assert_eq!(size_text(900_000, false), "0.9 MB");
        assert_eq!(size_text(949_999, false), "0.9 MB");
        assert_eq!(size_text(1_040_000, false), "1.0 MB");
        assert_eq!(size_text(1_040_000, true), "1.1 MB");
        assert_eq!(size_text(1_300_000, true), "1.3 MB");
        assert_eq!(size_text(12_345_678, false), "12.3 MB");
    }
}
