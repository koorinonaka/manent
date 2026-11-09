//! 録画フレームの差分圧縮ストア (`FrameStore`) と GIF 書き出し (`encode_gif_within`)。

// ponytail: 上限は時間のみ。変化の多い大画面で bytes() が膨らむなら byte 上限を足す

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::ops::Range;
use std::time::Duration;

use quantette::deps::palette::Srgb;
use quantette::{ImageRef, Pipeline};

use crate::settings::GIF_SCALES;

/// 復元済みの 1 フレーム。
pub struct Frame {
    pub bgra: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

struct Stored {
    timestamp: Duration,
    /// 書き換える矩形。キーフレームは全面。`None` は前フレームと同一。
    rect: Option<Rect>,
    /// 矩形の行を連結し lz4 で圧縮したもの。
    data: Vec<u8>,
}

pub struct FrameStore {
    width: u32,
    height: u32,
    keyframe_interval: usize,
    frames: Vec<Stored>,
    /// 最後に push したフレーム (差分の基準)。
    prev: Vec<u8>,
}

impl FrameStore {
    pub fn new(width: u32, height: u32, keyframe_interval: usize) -> Self {
        assert!(width > 0 && height > 0, "空の画面は録画しない");
        Self {
            width,
            height,
            keyframe_interval: keyframe_interval.max(1),
            frames: Vec::new(),
            prev: vec![0; width as usize * height as usize * 4],
        }
    }

    pub fn push(&mut self, bgra: &[u8], timestamp: Duration) {
        assert_eq!(bgra.len(), self.prev.len(), "BGRA の長さが画面サイズと違う");
        let w = self.width as usize;
        let rect = if self.frames.len().is_multiple_of(self.keyframe_interval) {
            Some(self.full())
        } else {
            diff_rect(&self.prev, bgra, w)
        };
        let data = match rect {
            Some(r) => {
                let rows = extract(bgra, w, r);
                put(&mut self.prev, w, r, &rows);
                lz4_flex::compress_prepend_size(&rows)
            }
            None => Vec::new(),
        };
        self.frames.push(Stored {
            timestamp,
            rect,
            data,
        });
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// 直前のキーフレームから差分を当てて復元する。
    pub fn frame(&self, i: usize) -> Frame {
        let mut bgra = vec![0; self.prev.len()];
        self.frame_into(i, &mut bgra, None);
        Frame { bgra }
    }

    /// `buf` をフレーム `i` にする。`have` は `buf` に今入っているフレーム。それが `i` より前で
    /// 同じキーフレームの区間にあれば、差分だけ当てる (再生とシークの順送り)。
    pub fn frame_into(&self, i: usize, buf: &mut [u8], have: Option<usize>) {
        let key = i - i % self.keyframe_interval;
        let from = match have {
            Some(h) if h == i => return,
            Some(h) if (key..i).contains(&h) => h + 1,
            _ => key,
        };
        for j in from..=i {
            self.apply(j, buf);
        }
    }

    /// 巻き戻し用: キーフレームから `i` まで 1 度だけ復元し、`i` で終わる最大 `n` 枚
    /// (キーフレームの区間内) を返す。戻り値は先頭のフレーム番号と、その順の画素。
    pub fn frames_back(&self, i: usize, n: usize) -> (usize, Vec<Vec<u8>>) {
        let key = i - i % self.keyframe_interval;
        let start = (i + 1).saturating_sub(n.max(1)).max(key);
        let mut buf = vec![0; self.prev.len()];
        let mut out = Vec::with_capacity(i + 1 - start);
        for j in key..=i {
            self.apply(j, &mut buf);
            if j >= start {
                out.push(buf.clone());
            }
        }
        (start, out)
    }

    /// 保持量 (圧縮データ + 差分の基準フレーム)。
    pub fn bytes(&self) -> usize {
        self.prev.len() + self.frames.iter().map(|f| f.data.len()).sum::<usize>()
    }

    fn full(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: self.width as usize,
            h: self.height as usize,
        }
    }

    fn apply(&self, i: usize, buf: &mut [u8]) {
        let s = &self.frames[i];
        if let Some(r) = s.rect {
            let rows = lz4_flex::decompress_size_prepended(&s.data).expect("自分で圧縮したデータ");
            put(buf, self.width as usize, r, &rows);
        }
    }
}

/// 2 フレームで値が違う画素の外接矩形。同一なら `None`。
fn diff_rect(a: &[u8], b: &[u8], width: usize) -> Option<Rect> {
    let stride = width * 4;
    let (mut x0, mut x1, mut y0, mut y1) = (usize::MAX, 0, usize::MAX, 0);
    for (y, (ra, rb)) in a
        .chunks_exact(stride)
        .zip(b.chunks_exact(stride))
        .enumerate()
    {
        if ra == rb {
            continue;
        }
        let px = || ra.chunks_exact(4).zip(rb.chunks_exact(4));
        x0 = x0.min(px().position(|(p, q)| p != q).unwrap_or(0));
        x1 = x1.max(px().rposition(|(p, q)| p != q).unwrap_or(width - 1));
        y0 = y0.min(y);
        y1 = y;
    }
    (y0 != usize::MAX).then(|| Rect {
        x: x0,
        y: y0,
        w: x1 - x0 + 1,
        h: y1 - y0 + 1,
    })
}

fn extract(src: &[u8], width: usize, r: Rect) -> Vec<u8> {
    let mut out = Vec::with_capacity(r.w * r.h * 4);
    for y in r.y..r.y + r.h {
        let s = (y * width + r.x) * 4;
        out.extend_from_slice(&src[s..s + r.w * 4]);
    }
    out
}

fn put(dst: &mut [u8], width: usize, r: Rect, rows: &[u8]) {
    for (row, y) in rows.chunks_exact(r.w * 4).zip(r.y..) {
        let s = (y * width + r.x) * 4;
        dst[s..s + row.len()].copy_from_slice(row);
    }
}

pub struct GifOptions {
    /// トリム。`store.len()` を超える分は切る。
    pub range: Range<usize>,
    /// 削除するフレーム。表示時間ごと落とす (GIF はその分短くなる)。
    pub deleted: BTreeSet<usize>,
    /// fps の上限。間引いたフレームの表示時間は直前の残るフレームが引き継ぐ。
    pub decimate_fps: Option<u32>,
    /// 録画を止めた時刻。最後のフレームはここまで出す。
    pub end: Duration,
    /// 書き出しの倍率 (%)。100 は等倍。縦横比は変えない。
    pub scale_pct: u32,
    /// 注釈。同じ注釈が出ているフレームの組ごとに 1 つ。空なら合成しない (今までと同じバイト列)。
    pub annots: Vec<AnnotSeg>,
}

/// 保存したフレームの添字の `frames` に乗せる注釈 (全寸)。
pub struct AnnotSeg {
    pub frames: Range<usize>,
    /// モザイク以外の注釈を描いた透明な層 (premultiplied BGRA)。モザイクだけなら None。
    pub layer: Option<Vec<u8>>,
    /// モザイクの矩形 (左, 上, 右, 下)。升はそのフレームの画素から計算する。
    pub mosaics: Vec<(u32, u32, u32, u32)>,
}

/// 倍率 `pct` で書き出したときの寸法: round(w × pct / 100) × round(h × pct / 100)、どちらも最小 1。
pub fn scaled_size(w: u32, h: u32, pct: u32) -> (u32, u32) {
    let f = |v: u32| ((u64::from(v) * u64::from(pct) + 50) / 100).max(1) as u32;
    (f(w), f(h))
}

/// BGRA を `pct` % に面積平均で縮める。100 以上は複製せずにそのまま返す。
/// ponytail: 出力の 1 画素は元の整数の箱 (x·w/w2 から (x+1)·w/w2 まで) の平均。端数の画素を按分しない。
fn downscale(bgra: &[u8], w: u32, h: u32, pct: u32) -> (Cow<'_, [u8]>, u32, u32) {
    let (w2, h2) = scaled_size(w, h, pct);
    if (w2, h2) == (w, h) {
        return (Cow::Borrowed(bgra), w, h);
    }
    let (w, h) = (w as usize, h as usize);
    let (ow, oh) = (w2 as usize, h2 as usize);
    let xs: Vec<usize> = (0..=ow).map(|x| x * w / ow).collect();
    let mut out = Vec::with_capacity(ow * oh * 4);
    let mut acc = vec![0u32; ow * 4];
    for oy in 0..oh {
        let (y0, y1) = (oy * h / oh, (oy + 1) * h / oh);
        acc.fill(0);
        for row in bgra[y0 * w * 4..y1 * w * 4].chunks_exact(w * 4) {
            for ox in 0..ow {
                let a = &mut acc[ox * 4..ox * 4 + 4];
                for px in row[xs[ox] * 4..xs[ox + 1] * 4].chunks_exact(4) {
                    for c in 0..4 {
                        a[c] += u32::from(px[c]);
                    }
                }
            }
        }
        for ox in 0..ow {
            let n = ((xs[ox + 1] - xs[ox]) * (y1 - y0)) as u32;
            out.extend(
                acc[ox * 4..ox * 4 + 4]
                    .iter()
                    .map(|&v| ((v + n / 2) / n) as u8),
            );
        }
    }
    (Cow::Owned(out), w2, h2)
}

/// 書き出すフレームと、それぞれの表示時間 (ns)。
/// 各フレームの自分の時間は次のフレームの時刻まで (最後のフレームは録画を止めた時刻まで。
/// `Timeline::own` と同じ規則)。総時間はトリムの中の削除していないフレームの自分の時間の合計。
fn plan_frames(store: &FrameStore, opts: &GifOptions) -> Vec<(usize, u128)> {
    let n = store.len();
    let end = opts.range.end.min(n);
    let ts = |i: usize| store.frames[i].timestamp.as_nanos();
    let own = |i: usize| {
        let next = if i + 1 < n {
            ts(i + 1)
        } else {
            opts.end.as_nanos()
        };
        next.saturating_sub(ts(i))
    };
    let fps = opts.decimate_fps.filter(|&f| f > 0).map(u128::from);
    let mut out: Vec<(usize, u128)> = Vec::new();
    // 削除を詰めた後の時刻。間引きの枠はこれで数え、詰めた GIF の上で fps の上限を守る。
    let (mut at, mut last_bucket) = (0u128, None);
    for i in (opts.range.start..end).filter(|i| !opts.deleted.contains(i)) {
        // 1/fps 秒の枠ごとに先頭の 1 枚だけ残す。整数で数えるので 1/30 秒の端数で枠を取りこぼさない。
        let bucket = fps.map(|f| at * f / 1_000_000_000);
        match out.last_mut() {
            Some(last) if bucket.is_some() && bucket == last_bucket => last.1 += own(i),
            _ => out.push((i, own(i))),
        }
        last_bucket = bucket;
        at += own(i);
    }
    out
}

/// トリム・削除・fps 間引き・倍率を当てて GIF を作る。無限ループ。フレームごとにローカルパレット。
/// 倍率は復元したフレームを縮めてから当て、差分と量子化は縮めた画素で行う。
/// `progress(書いたフレーム数, このエンコードのフレーム数)` を、始めに 0 で、以後フレームを 1 枚書くごとに呼ぶ。
fn encode_gif_with(
    store: &FrameStore,
    opts: &GifOptions,
    progress: &mut dyn FnMut(usize, usize),
) -> Vec<u8> {
    crate::insight_scope!("gif_encode");
    let plan = plan_frames(store, opts);
    progress(0, plan.len());
    let (sw, sh) = scaled_size(store.width, store.height, opts.scale_pct);
    let w = sw as usize;
    let mut out = Vec::new();
    let mut enc = gif::Encoder::new(&mut out, sw as u16, sh as u16, &[])
        .expect("Vec への書き込みは失敗しない");
    enc.set_repeat(gif::Repeat::Infinite)
        .expect("Vec への書き込みは失敗しない");

    let mut cur: Vec<u8> = Vec::new();
    let mut cur_idx: Option<usize> = None;
    // 直前に書き出したフレームの元画素。変わった矩形だけを次のフレームに書く。
    let mut shown: Option<Vec<u8>> = None;
    let (mut elapsed_ns, mut elapsed_cs) = (0u128, 0u128);

    for (k, &(idx, dur)) in plan.iter().enumerate() {
        match cur_idx {
            Some(c) if c < idx => (c + 1..=idx).for_each(|j| store.apply(j, &mut cur)),
            _ => cur = store.frame(idx).bgra,
        }
        cur_idx = Some(idx);
        // 注釈は `cur` (次のフレームの差分の基準) を汚さないよう複製に描き、その後で縮める。
        let drawn = opts
            .annots
            .iter()
            .find(|s| s.frames.contains(&idx))
            .map(|s| {
                let mut img = crate::capture::Image {
                    w: store.width,
                    h: store.height,
                    bgra: cur.clone(),
                };
                crate::annot::overlay(&mut img, s.layer.as_deref(), &s.mosaics);
                img.bgra
            });
        let full = drawn.as_deref().unwrap_or(&cur);
        let (small, _, _) = downscale(full, store.width, store.height, opts.scale_pct);

        let rect = match &shown {
            None => Rect {
                x: 0,
                y: 0,
                w,
                h: sh as usize,
            },
            // 同一なら 1 画素だけ書き直す (GIF のフレームは 1 画素以上要る)。
            Some(prev) => diff_rect(prev, &small, w).unwrap_or(Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            }),
        };
        let rows = extract(&small, w, rect);
        match &mut shown {
            Some(prev) => put(prev, w, rect, &rows),
            None => shown = Some(small.to_vec()),
        }

        let pixels: Vec<Srgb<u8>> = rows
            .chunks_exact(4)
            .map(|p| Srgb::new(p[2], p[1], p[0]))
            .collect();
        let image = ImageRef::new(rect.w as u32, rect.h as u32, pixels.as_slice())
            .expect("矩形と画素数は一致する");
        // ディザ無し: 画面録画では静止部分がちらつき、差分も大きくなる。
        let (palette, indices) = Pipeline::new()
            .ditherer(None)
            .parallel(true)
            .input_image(image)
            .output_srgb8_indexed_image()
            .into_parts();

        // 遅延は 1/100 秒単位。累積時間を丸めて差を取るので端数の誤差は持ち越される。
        elapsed_ns += dur;
        let cs = (elapsed_ns + 5_000_000) / 10_000_000;
        let delay = u16::try_from(cs - elapsed_cs).unwrap_or(u16::MAX);
        elapsed_cs = cs;

        let frame = gif::Frame {
            left: rect.x as u16,
            top: rect.y as u16,
            width: rect.w as u16,
            height: rect.h as u16,
            delay,
            dispose: gif::DisposalMethod::Keep,
            palette: Some(
                palette
                    .iter()
                    .flat_map(|c| [c.red, c.green, c.blue])
                    .collect(),
            ),
            buffer: Cow::Owned(indices),
            ..Default::default()
        };
        enc.write_frame(&frame)
            .expect("Vec への書き込みは失敗しない");
        progress(k + 1, plan.len());
    }
    enc.into_inner().expect("Vec への書き込みは失敗しない");
    out
}

/// サイズ上限に収めた GIF と、収めるのに使った倍率と fps の上限。
pub struct Fitted {
    pub bytes: Vec<u8>,
    pub scale_pct: u32,
    pub decimate_fps: Option<u32>,
    /// 下げ切っても上限を超えた。
    pub over: bool,
    /// 上限を超えて下げる段を通ったときの、最後の fps (間引かないなら録画の fps 相当)。通らなければ `None`。
    pub reduced_fps: Option<u32>,
}

/// 上限 `limit` バイト (`None` は上限なし) に収まるまで、fps → 倍率 → fps の順に下げて焼き直す。
/// 順は trim-shot の `GifConverter.ReduceToTarget` と同じで、寸法を最後まで残す。`opts` の倍率と fps は上限で、上げない。
/// 見積りは直前に焼いた実測から、バイト数がフレーム数と画素数に比例するとして外挿する。
/// fps を下げると残るフレームの差分が大きくなるので、外挿は甘く出る。その分は焼き直しの実測で詰める。
/// `progress(何回目, 書いたフレーム数, そのエンコードのフレーム数)` は回ごとに 0 から呼ぶ (1 回目が 1)。
/// ponytail: 最悪 4 回フルにエンコードする。長い録画で遅ければ trim-shot のサンプル窓のモデルを移す。
pub fn encode_gif_within(
    store: &FrameStore,
    mut opts: GifOptions,
    limit: Option<usize>,
    progress: &mut dyn FnMut(u32, usize, usize),
) -> Fitted {
    let mut bytes = encode_gif_with(store, &opts, &mut |d, n| progress(1, d, n));
    let done = |bytes: Vec<u8>, opts: &GifOptions, limit: Option<usize>| Fitted {
        over: limit.is_some_and(|l| bytes.len() > l),
        bytes,
        scale_pct: opts.scale_pct,
        decimate_fps: opts.decimate_fps,
        reduced_fps: None,
    };
    let Some(limit) = limit.filter(|&l| bytes.len() > l) else {
        return done(bytes, &opts, limit);
    };
    // 間引き無しの fps (録画の fps 相当) と、fps ごとの書き出す枚数 (`counts[f]`)。
    let user_fps = opts.decimate_fps.take();
    let plan = plan_frames(store, &opts);
    let secs = plan.iter().map(|f| f.1).sum::<u128>() as f64 / 1e9;
    let native = if secs > 0.0 {
        ((plan.len() as f64 / secs).ceil() as u32).max(1)
    } else {
        1
    };
    let mut counts = vec![0usize; native as usize + 1];
    counts[native as usize] = plan.len();
    for f in 1..native {
        opts.decimate_fps = Some(f);
        counts[f as usize] = plan_frames(store, &opts).len();
    }
    let area = |s: u32| {
        let (w, h) = scaled_size(store.width, store.height, s);
        f64::from(w) * f64::from(h)
    };
    let (mut scale, mut fps) = (opts.scale_pct, user_fps.unwrap_or(native).min(native));
    let target = limit as f64 * 0.93;
    for pass in 2..=4 {
        let (b, s0, f0) = (bytes.len() as f64, scale, fps);
        let est = |s: u32, f: u32| {
            b * counts[f as usize] as f64 / counts[f0 as usize].max(1) as f64 * area(s) / area(s0)
        };
        let next = reduce(scale, fps, |s, f| est(s, f) <= target);
        if next == (scale, fps) {
            break;
        }
        (scale, fps) = next;
        opts.scale_pct = scale;
        opts.decimate_fps = (fps < native).then_some(fps);
        bytes = encode_gif_with(store, &opts, &mut |d, n| progress(pass, d, n));
        if bytes.len() <= limit {
            break;
        }
    }
    opts.scale_pct = scale;
    opts.decimate_fps = (fps < native).then_some(fps);
    Fitted {
        reduced_fps: Some(fps),
        ..done(bytes, &opts, Some(limit))
    }
}

/// 収まる (`fits`) まで fps を 10 へ → 倍率を `GIF_SCALES` で 25 へ → fps を 1 へ、の順に下げる。
/// どの段も今より上げない。収まらなければ下限まで下げた組を返す。
fn reduce(scale: u32, fps: u32, fits: impl Fn(u32, u32) -> bool) -> (u32, u32) {
    let (mut s, mut f) = (scale, fps);
    let down = |cur: u32, floor: u32, ok: &dyn Fn(u32) -> bool| {
        let floor = floor.min(cur);
        (floor..=cur).rev().find(|&v| ok(v)).unwrap_or(floor)
    };
    if !fits(s, f) {
        f = down(f, 10, &|v| fits(s, v));
    }
    if !fits(s, f) {
        let ladder = || GIF_SCALES.iter().copied().filter(|&v| v <= s);
        s = ladder()
            .find(|&v| fits(v, f))
            .or(ladder().next_back())
            .unwrap_or(s);
    }
    if !fits(s, f) {
        f = down(f, 1, &|v| fits(s, v));
    }
    (s, f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_gif(store: &FrameStore, opts: &GifOptions) -> Vec<u8> {
        encode_gif_with(store, opts, &mut |_, _| {})
    }

    fn xorshift(state: &mut u32) -> u32 {
        *state ^= *state << 13;
        *state ^= *state >> 17;
        *state ^= *state << 5;
        *state
    }

    /// ノイズの背景に、フレームごとに位置の変わる矩形を描いた BGRA 列。
    fn moving_box(w: usize, h: usize, n: usize, seed: u32) -> Vec<Vec<u8>> {
        let mut s = seed;
        let base: Vec<u8> = (0..w * h * 4).map(|_| xorshift(&mut s) as u8).collect();
        (0..n)
            .map(|i| {
                let mut f = base.clone();
                for y in (i % (h / 2))..(i % (h / 2) + h / 4) {
                    for x in (i % (w / 2))..(i % (w / 2) + w / 4) {
                        let p = (y * w + x) * 4;
                        f[p..p + 4].copy_from_slice(&[(i * 7) as u8, 200, 30, 255]);
                    }
                }
                f
            })
            .collect()
    }

    fn ts30(i: usize) -> Duration {
        Duration::from_secs_f64(i as f64 / 30.0)
    }

    #[test]
    fn frames_round_trip_for_every_keyframe_interval() {
        // 全面ノイズ (毎フレーム全面が変わる) と部分変化の両方を通す。
        let mut s = 7u32;
        let mut frames = moving_box(17, 9, 12, 3);
        frames.extend((0..5).map(|_| (0..17 * 9 * 4).map(|_| xorshift(&mut s) as u8).collect()));
        frames.push(frames[16].clone()); // 前フレームと同一
        for interval in [1, 4, 100] {
            let mut store = FrameStore::new(17, 9, interval);
            for (i, f) in frames.iter().enumerate() {
                store.push(f, ts30(i));
            }
            assert_eq!(store.len(), frames.len());
            for (i, f) in frames.iter().enumerate() {
                let got = store.frame(i);
                assert!(got.bgra == *f, "interval {interval} frame {i}");
            }
            // 順送り・飛び・戻りの差分適用も全面の復元と同じになる。
            let mut buf = vec![0; 17 * 9 * 4];
            let mut have = None;
            for i in [0, 1, 2, 5, 3, 16, 17, 17, 8, 9, 12] {
                store.frame_into(i, &mut buf, have);
                assert!(
                    buf == frames[i],
                    "interval {interval} into {i} from {have:?}"
                );
                have = Some(i);
            }
            // 巻き戻しの先読みも同じ画素を、`i` で終わる区間内の順に返す。
            for (i, n) in [(0, 3), (2, 2), (3, 10), (7, 1), (17, 5)] {
                let (a, got) = store.frames_back(i, n);
                assert_eq!(a + got.len(), i + 1, "interval {interval} back {i}");
                assert!(got.len() <= n && a >= i - i % interval);
                for (k, g) in got.iter().enumerate() {
                    assert!(
                        *g == frames[a + k],
                        "interval {interval} back {i} at {}",
                        a + k
                    );
                }
            }
        }
    }

    #[test]
    fn partial_change_1080p_stores_under_a_tenth_of_raw() {
        let (w, h) = (1920usize, 1080usize);
        let mut s = 11u32;
        let base: Vec<u8> = (0..w * h * 4).map(|_| xorshift(&mut s) as u8).collect();
        let mut store = FrameStore::new(w as u32, h as u32, 30);
        let mut f = base.clone();
        for i in 0..60 {
            // 200x100 の矩形が右へ動く。背景はノイズで lz4 が効かない。
            for y in 400..500 {
                for x in 0..200 {
                    let p = (y * w + x + i * 10) * 4;
                    f[p..p + 4].copy_from_slice(&[i as u8, 255, 0, 255]);
                }
            }
            store.push(&f, ts30(i));
        }
        let raw = w * h * 4 * 60;
        assert!(store.bytes() * 10 < raw, "{} / {raw}", store.bytes());
    }

    struct Decoded {
        frames: usize,
        delay_cs: u32,
        infinite: bool,
        size: (u16, u16),
        /// フレームごとの遅延 (1/100 秒)。
        delays: Vec<u16>,
    }

    fn decode(gif_bytes: &[u8]) -> Decoded {
        let mut d = gif::DecodeOptions::new()
            .read_info(std::io::Cursor::new(gif_bytes))
            .unwrap();
        let (mut frames, mut delay_cs, mut delays) = (0, 0u32, Vec::new());
        let size = (d.width(), d.height());
        while let Some(f) = d.read_next_frame().unwrap() {
            frames += 1;
            delay_cs += u32::from(f.delay);
            delays.push(f.delay);
        }
        Decoded {
            frames,
            delay_cs,
            infinite: d.repeat() == gif::Repeat::Infinite,
            size,
            delays,
        }
    }

    fn store_of(frames: &[Vec<u8>], w: u32, h: u32, ts: impl Fn(usize) -> Duration) -> FrameStore {
        let mut store = FrameStore::new(w, h, 8);
        for (i, f) in frames.iter().enumerate() {
            store.push(f, ts(i));
        }
        store
    }

    #[test]
    fn gif_frame_count_and_total_delay_follow_trim_and_delete() {
        // 間隔がばらつく timestamp (25ms〜58ms)。
        let t = |i: usize| Duration::from_millis((i * 37 + (i * i) % 23) as u64);
        let frames = moving_box(64, 48, 40, 5);
        let store = store_of(&frames, 64, 48, t);
        let opts = GifOptions {
            range: 5..30,
            deleted: BTreeSet::from([5, 12, 13, 29]),
            decimate_fps: None,
            end: t(40),
            scale_pct: 100,
            annots: Vec::new(),
        };
        let gif_bytes = encode_gif(&store, &opts);
        let d = decode(&gif_bytes);
        assert_eq!(d.frames, 25 - 4);
        assert!(d.infinite);
        // 残る最初のフレーム (6) からトリム終端の次のフレーム (30) まで。削除した 12・13・29 の時間は詰める。
        let own = |i: usize| t(i + 1) - t(i);
        let elapsed_cs = ((t(30) - t(6)) - own(12) - own(13) - own(29)).as_secs_f64() * 100.0;
        assert!(
            (f64::from(d.delay_cs) - elapsed_cs).abs() <= 1.0,
            "{} vs {elapsed_cs}",
            d.delay_cs
        );

        std::fs::create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/artifacts")).unwrap();
        std::fs::write(
            concat!(env!("CARGO_MANIFEST_DIR"), "/artifacts/clip_test.gif"),
            &gif_bytes,
        )
        .unwrap();
    }

    #[test]
    fn decimate_30fps_to_10fps_keeps_a_third_and_total_time() {
        let frames = moving_box(64, 48, 60, 9);
        let store = store_of(&frames, 64, 48, ts30);
        let all = decode(&encode_gif(
            &store,
            &GifOptions {
                range: 0..60,
                deleted: BTreeSet::new(),
                decimate_fps: None,
                end: ts30(60),
                scale_pct: 100,
                annots: Vec::new(),
            },
        ));
        let dec = decode(&encode_gif(
            &store,
            &GifOptions {
                range: 0..60,
                deleted: BTreeSet::new(),
                decimate_fps: Some(10),
                end: ts30(60),
                scale_pct: 100,
                annots: Vec::new(),
            },
        ));
        assert_eq!(all.frames, 60);
        assert!(dec.frames.abs_diff(20) <= 1, "{}", dec.frames);
        // 60 フレーム × 1/30 秒 = 200cs。
        assert!(all.delay_cs.abs_diff(200) <= 1, "{}", all.delay_cs);
        assert!(dec.delay_cs.abs_diff(200) <= 1, "{}", dec.delay_cs);
        // 間引きの枠の中のフレーム (枠の先頭でない 1・4・7) を削除すると、その時間は合計から減る。
        // 57 枚 × 1/30 秒 = 190cs。枠は詰めた後の時刻で数えるので 10fps で 19 枚。
        let del = decode(&encode_gif(
            &store,
            &GifOptions {
                range: 0..60,
                deleted: BTreeSet::from([1, 4, 7]),
                decimate_fps: Some(10),
                end: ts30(60),
                scale_pct: 100,
                annots: Vec::new(),
            },
        ));
        assert!(del.delay_cs.abs_diff(190) <= 1, "{}", del.delay_cs);
        assert!(del.frames.abs_diff(19) <= 1, "{}", del.frames);
    }

    #[test]
    fn scaled_size_rounds_and_keeps_at_least_one() {
        // カードの見本: 560 × 315 の 50 % は 280 × 158。
        assert_eq!(scaled_size(560, 315, 50), (280, 158));
        assert_eq!(scaled_size(1, 1, 25), (1, 1));
        assert_eq!(scaled_size(3, 3, 33), (1, 1));
        assert_eq!(scaled_size(1920, 1080, 100), (1920, 1080));
    }

    #[test]
    fn downscale_averages_each_box() {
        // 4×4 を 50 %: 2×2 の箱ごとの平均 (四捨五入)。チャネルごとに B G R A。
        let px = |v: u8| [v, v / 2, 255 - v, 255];
        let vals: [[u8; 4]; 4] = [
            [0, 10, 100, 101],
            [20, 30, 102, 104],
            [200, 200, 1, 2],
            [200, 201, 3, 4],
        ];
        let src: Vec<u8> = vals.iter().flatten().flat_map(|&v| px(v)).collect();
        let (out, w, h) = downscale(&src, 4, 4, 50);
        assert_eq!((w, h), (2, 2));
        // 左上 (0,10,20,30) → B 15、G (0+5+10+15)/4=7.5→8、R 240。
        // 右上 (100,101,102,104) → B 101.75→102、G (50+50+51+52)/4=50.75→51、R 153.25→153。
        // 左下 (200,200,200,201) → B 200.25→200、G (100+100+100+100)/4=100、R 54.75→55。
        // 右下 (1,2,3,4) → B 2.5→3、G (0+1+1+2)/4=1、R 252.5→253。
        let want: Vec<u8> = [
            [15, 8, 240, 255],
            [102, 51, 153, 255],
            [200, 100, 55, 255],
            [3, 1, 253, 255],
        ]
        .concat();
        assert_eq!(&*out, &want[..]);

        // 3×3 を 33 %: 1×1。9 画素の平均。
        let src: Vec<u8> = (0..9u8).flat_map(|i| [i * 10, 0, 90, 255]).collect();
        let (out, w, h) = downscale(&src, 3, 3, 33);
        assert_eq!((w, h), (1, 1));
        assert_eq!(&*out, &[40, 0, 90, 255]);

        // 100 % は複製しない。
        assert!(matches!(downscale(&src, 3, 3, 100).0, Cow::Borrowed(_)));
    }

    /// 上限を超えたら fps を 10 まで下げてから倍率を下げ、それでも足りなければ fps を 1 へ下げる。今より上げない。
    #[test]
    fn reduce_lowers_fps_to_10_before_scale_and_never_raises() {
        // 見積り = 倍率 × fps。
        let under = |cap: u32| move |s: u32, f: u32| s * f <= cap;
        assert_eq!(reduce(100, 30, under(3000)), (100, 30));
        assert_eq!(reduce(100, 30, under(2000)), (100, 20));
        assert_eq!(reduce(100, 30, under(500)), (50, 10));
        assert_eq!(reduce(100, 30, under(100)), (25, 4));
        assert_eq!(reduce(100, 30, under(1)), (25, 1));
        assert_eq!(reduce(33, 8, under(200)), (25, 8));
    }

    /// 等倍の 3 分の 1 の上限を付けると、焼き直して上限の中に収める。上限が無ければ等倍のまま。
    #[test]
    fn encode_within_limit_fits_under_the_limit() {
        let frames = moving_box(64, 48, 30, 13);
        let store = store_of(&frames, 64, 48, ts30);
        let opts = || GifOptions {
            range: 0..30,
            deleted: BTreeSet::new(),
            decimate_fps: None,
            end: ts30(30),
            scale_pct: 100,
            annots: Vec::new(),
        };
        let full = encode_gif_within(&store, opts(), None, &mut |_, _, _| {});
        assert!(!full.over);
        assert_eq!((full.scale_pct, full.decimate_fps), (100, None));
        let limit = full.bytes.len() / 3;
        let fit = encode_gif_within(&store, opts(), Some(limit), &mut |_, _, _| {});
        assert!(
            !fit.over && fit.bytes.len() <= limit,
            "{} > {limit} at {}% {:?}",
            fit.bytes.len(),
            fit.scale_pct,
            fit.decimate_fps
        );
        assert!(fit.scale_pct < 100 || fit.decimate_fps.is_some());
    }

    /// 進捗は回ごとに 0 からそのエンコードのフレーム数まで 1 ずつ進み、焼き直すと回数が 1 つ増える。
    /// 最後の回の全体は、書き出した GIF のフレーム数と同じ。
    #[test]
    fn encode_within_progress_restarts_from_zero_each_pass() {
        let frames = moving_box(64, 48, 30, 13);
        let store = store_of(&frames, 64, 48, ts30);
        let opts = || GifOptions {
            range: 0..30,
            deleted: BTreeSet::new(),
            decimate_fps: None,
            end: ts30(30),
            scale_pct: 100,
            annots: Vec::new(),
        };
        let full = encode_gif(&store, &opts()).len();
        let mut calls: Vec<(u32, usize, usize)> = Vec::new();
        let fit = encode_gif_within(&store, opts(), Some(full / 3), &mut |p, d, n| {
            calls.push((p, d, n))
        });
        let passes: Vec<u32> = calls.iter().map(|c| c.0).collect();
        let last_pass = *passes.last().unwrap();
        assert!(last_pass >= 2, "no re-encode: {passes:?}");
        let mut at = 0;
        for pass in 1..=last_pass {
            let run: Vec<_> = calls[at..].iter().take_while(|c| c.0 == pass).collect();
            assert!(!run.is_empty(), "pass {pass} missing: {passes:?}");
            let n = run[0].2;
            let expect: Vec<usize> = (0..=n).collect();
            assert_eq!(
                run.iter().map(|c| c.1).collect::<Vec<_>>(),
                expect,
                "pass {pass}"
            );
            assert!(run.iter().all(|c| c.2 == n));
            if pass == 1 {
                assert_eq!(n, 30);
            }
            at += run.len();
        }
        assert_eq!(at, calls.len());
        assert_eq!(calls.last().unwrap().2, decode(&fit.bytes).frames);
        assert!(fit.reduced_fps.is_some());
    }

    #[test]
    fn half_scale_gif_halves_screen_and_keeps_frames_and_delays() {
        let t = |i: usize| Duration::from_millis((i * 37 + (i * i) % 23) as u64);
        let frames = moving_box(64, 48, 30, 13);
        let store = store_of(&frames, 64, 48, t);
        let at = |scale_pct| {
            decode(&encode_gif(
                &store,
                &GifOptions {
                    range: 2..28,
                    deleted: BTreeSet::from([4, 9]),
                    decimate_fps: None,
                    end: t(30),
                    scale_pct,
                    annots: Vec::new(),
                },
            ))
        };
        let full = at(100);
        assert_eq!(full.size, (64, 48));
        for (pct, size) in [(50, (32, 24)), (25, (16, 12)), (33, (21, 16))] {
            let d = at(pct);
            assert_eq!(d.size, size, "{pct} %");
            assert_eq!(d.frames, full.frames, "{pct} %");
            assert_eq!(d.delays, full.delays, "{pct} %");
        }
    }

    /// 縮めた GIF の画素が、元のフレームを縮めた画素と同じになる (差分の矩形も縮めた画素で取っている)。
    /// 単色の面と 2 px 刻みで動く 8 × 8 の箱なので、50 % の箱の平均は色を混ぜず、量子化でも色が変わらない。
    #[test]
    fn half_scale_gif_pixels_match_downscaled_frames() {
        let (w, h) = (64usize, 48usize);
        let frames: Vec<Vec<u8>> = (0..12)
            .map(|i| {
                let (bx, by) = (2 * i, 2 * i);
                (0..w * h)
                    .flat_map(|k| {
                        let (x, y) = (k % w, k / w);
                        if (bx..bx + 8).contains(&x) && (by..by + 8).contains(&y) {
                            [40, 200, 30, 255]
                        } else if x < w / 2 {
                            [0, 0, 200, 255]
                        } else {
                            [200, 100, 0, 255]
                        }
                    })
                    .collect()
            })
            .collect();
        let store = store_of(&frames, w as u32, h as u32, ts30);
        let gif_bytes = encode_gif(
            &store,
            &GifOptions {
                range: 0..12,
                deleted: BTreeSet::new(),
                decimate_fps: None,
                end: ts30(12),
                scale_pct: 50,
                annots: Vec::new(),
            },
        );
        let mut opts = gif::DecodeOptions::new();
        opts.set_color_output(gif::ColorOutput::RGBA);
        let mut d = opts.read_info(std::io::Cursor::new(&gif_bytes)).unwrap();
        let (cw, ch) = (usize::from(d.width()), usize::from(d.height()));
        assert_eq!((cw, ch), (32, 24));
        // 前のフレームの上に重ねる (DisposalMethod::Keep)。
        let mut canvas = vec![0u8; cw * ch * 4];
        let mut i = 0;
        while let Some(f) = d.read_next_frame().unwrap() {
            let (l, t, fw) = (
                usize::from(f.left),
                usize::from(f.top),
                usize::from(f.width),
            );
            for (row, y) in f.buffer.chunks_exact(fw * 4).zip(t..) {
                canvas[(y * cw + l) * 4..(y * cw + l + fw) * 4].copy_from_slice(row);
            }
            // 縮めた後: 箱は (i, i) から 4 × 4、左半分 (x < 16) は赤、右半分は青 (RGBA)。
            let rgba: Vec<u8> = (0..cw * ch)
                .flat_map(|k| {
                    let (x, y) = (k % cw, k / cw);
                    if (i..i + 4).contains(&x) && (i..i + 4).contains(&y) {
                        [30, 200, 40, 255]
                    } else if x < cw / 2 {
                        [200, 0, 0, 255]
                    } else {
                        [0, 100, 200, 255]
                    }
                })
                .collect();
            assert!(canvas == rgba, "frame {i}");
            i += 1;
        }
        assert_eq!(i, 12);
    }

    /// GIF を RGBA で読み、各フレームを前のフレームに重ねた全面の画素 (DisposalMethod::Keep)。
    fn canvases(gif_bytes: &[u8]) -> (usize, usize, Vec<Vec<u8>>) {
        let mut opts = gif::DecodeOptions::new();
        opts.set_color_output(gif::ColorOutput::RGBA);
        let mut d = opts.read_info(std::io::Cursor::new(gif_bytes)).unwrap();
        let (cw, ch) = (usize::from(d.width()), usize::from(d.height()));
        let mut canvas = vec![0u8; cw * ch * 4];
        let mut out = Vec::new();
        while let Some(f) = d.read_next_frame().unwrap() {
            let (l, t, fw) = (
                usize::from(f.left),
                usize::from(f.top),
                usize::from(f.width),
            );
            for (row, y) in f.buffer.chunks_exact(fw * 4).zip(t..) {
                canvas[(y * cw + l) * 4..(y * cw + l + fw) * 4].copy_from_slice(row);
            }
            out.push(canvas.clone());
        }
        (cw, ch, out)
    }

    fn solid(w: usize, h: usize, bgra: [u8; 4]) -> Vec<u8> {
        bgra.repeat(w * h)
    }

    fn rgba_at(c: &[u8], cw: usize, x: usize, y: usize) -> [u8; 4] {
        let i = (y * cw + x) * 4;
        [c[i], c[i + 1], c[i + 2], c[i + 3]]
    }

    #[test]
    fn gif_export_annotation_only_in_interval_frames() {
        let (w, h) = (32usize, 16usize);
        // 毎フレーム同じ灰色の地。注釈の層は (8,4)–(16,12) の不透明な赤 (BGRA)。
        let frames: Vec<Vec<u8>> = (0..30).map(|_| solid(w, h, [90, 90, 90, 255])).collect();
        let store = store_of(&frames, w as u32, h as u32, ts30);
        let mut layer = vec![0u8; w * h * 4];
        for y in 4..12 {
            for x in 8..16 {
                layer[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[0, 0, 255, 255]);
            }
        }
        let segs = crate::annot::segments(&[(10, 20)], 0..30);
        assert_eq!(segs, vec![(10..21, vec![0])]);
        let at = |scale_pct| {
            let annots = segs
                .iter()
                .map(|(r, _)| AnnotSeg {
                    frames: r.clone(),
                    layer: Some(layer.clone()),
                    mosaics: Vec::new(),
                })
                .collect();
            encode_gif(
                &store,
                &GifOptions {
                    range: 0..30,
                    deleted: BTreeSet::new(),
                    decimate_fps: None,
                    end: ts30(30),
                    scale_pct,
                    annots,
                },
            )
        };
        let (cw, _, full) = canvases(&at(100));
        assert_eq!(full.len(), 30);
        for (i, c) in full.iter().enumerate() {
            let want = if (10..=20).contains(&i) {
                [255, 0, 0, 255]
            } else {
                [90, 90, 90, 255]
            };
            assert_eq!(rgba_at(c, cw, 8, 4), want, "frame {i} inside");
            assert_eq!(rgba_at(c, cw, 15, 11), want, "frame {i} inside corner");
            assert_eq!(rgba_at(c, cw, 7, 4), [90, 90, 90, 255], "frame {i} outside");
        }
        // 50 %: 注釈を合成した全寸のフレームを縮めた画素 (矩形は 2 px 刻みなので色は混ざらない)。
        let (cw, ch, half) = canvases(&at(50));
        assert_eq!((cw, ch), (16, 8));
        for (i, c) in half.iter().enumerate() {
            let inside = (10..=20).contains(&i);
            let want: Vec<u8> = (0..cw * ch)
                .flat_map(|k| {
                    let (x, y) = (k % cw, k / cw);
                    if inside && (4..8).contains(&x) && (2..6).contains(&y) {
                        [255, 0, 0, 255]
                    } else {
                        [90, 90, 90, 255]
                    }
                })
                .collect();
            assert!(*c == want, "half frame {i}");
        }
    }

    #[test]
    fn gif_export_without_annotations_is_unchanged() {
        // `gif_frame_count_and_total_delay_follow_trim_and_delete` と同じ入力。期待値はこのタスクの前
        // (3f8e1bc) の `encode_gif` が書いた artifacts/clip_test.gif の長さと FNV-1a 64。
        let t = |i: usize| Duration::from_millis((i * 37 + (i * i) % 23) as u64);
        let store = store_of(&moving_box(64, 48, 40, 5), 64, 48, t);
        let bytes = encode_gif(
            &store,
            &GifOptions {
                range: 5..30,
                deleted: BTreeSet::from([5, 12, 13, 29]),
                decimate_fps: None,
                end: t(40),
                scale_pct: 100,
                annots: Vec::new(),
            },
        );
        let fnv = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        assert_eq!((bytes.len(), fnv), (9523, 0x9236_2ae0_1d0b_e835));
    }

    #[test]
    fn gif_export_mosaic_follows_each_frame() {
        let (w, h) = (16usize, 16usize);
        // フレーム 0 は白黒の市松 (升の平均は 128 の灰)、フレーム 1 は一面の青。
        let checker: Vec<u8> = (0..w * h)
            .flat_map(|k| {
                if (k % w + k / w) % 2 == 0 {
                    [0, 0, 0, 255]
                } else {
                    [255, 255, 255, 255]
                }
            })
            .collect();
        let frames = vec![checker, solid(w, h, [200, 0, 0, 255])];
        let store = store_of(&frames, w as u32, h as u32, ts30);
        let bytes = encode_gif(
            &store,
            &GifOptions {
                range: 0..2,
                deleted: BTreeSet::new(),
                decimate_fps: None,
                end: ts30(2),
                scale_pct: 100,
                annots: vec![AnnotSeg {
                    frames: 0..2,
                    layer: None,
                    mosaics: vec![(0, 0, 8, 8)],
                }],
            },
        );
        let (cw, _, c) = canvases(&bytes);
        for (x, y) in [(0, 0), (7, 7), (3, 5)] {
            assert_eq!(
                rgba_at(&c[0], cw, x, y),
                [128, 128, 128, 255],
                "frame 0 ({x},{y})"
            );
            assert_eq!(
                rgba_at(&c[1], cw, x, y),
                [0, 0, 200, 255],
                "frame 1 ({x},{y})"
            );
        }
        // 範囲の外は元の画素のまま。
        assert_eq!(rgba_at(&c[0], cw, 8, 0), [0, 0, 0, 255]);
    }
}
