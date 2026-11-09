//! GIF ピンのタイムラインの状態 (今のフレーム・範囲選択・トリム・削除) と、そこから作る `GifOptions`。
//! 添字は 0 始まり、範囲は両端を含む。画面に出す番号は 1 始まり (DS: 180–240 = 61 フレーム)。

use std::collections::BTreeSet;
use std::time::Duration;

use crate::clip::GifOptions;

pub struct Timeline {
    /// 各フレームの録画開始からの時刻。
    pub ts: Vec<Duration>,
    pub cur: usize,
    /// Shift+クリックの起点。
    anchor: Option<usize>,
    pub sel: Option<(usize, usize)>,
    pub trim: (usize, usize),
    pub deleted: BTreeSet<usize>,
    /// 録画を止めた時刻。最後のフレームはここまで出す。
    pub end: Duration,
    /// 削除の前後の状態 (削除の集合、今、範囲)。元に戻す・やり直すで行き来する。
    undo: Vec<Snap>,
    redo: Vec<Snap>,
}

type Snap = (BTreeSet<usize>, usize, Option<(usize, usize)>);

impl Timeline {
    pub fn new(ts: Vec<Duration>, end: Duration) -> Self {
        assert!(!ts.is_empty(), "フレームの無い録画はピンにしない");
        let last = ts.len() - 1;
        Self {
            ts,
            cur: 0,
            anchor: None,
            sel: None,
            trim: (0, last),
            deleted: BTreeSet::new(),
            end,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.ts.len()
    }

    pub fn kept(&self, i: usize) -> bool {
        !self.deleted.contains(&i)
    }

    /// 注釈の区間 `span` (保存したフレームの添字、両端を含む) の実効の区間: 削除されておらず
    /// トリムの中のフレーム。空なら、その注釈は消えたものとして扱う。区間の数値は詰めない。
    pub fn effective(&self, (a, b): (usize, usize)) -> impl Iterator<Item = usize> + '_ {
        (a..=b.min(self.len() - 1)).filter(|&j| self.kept(j) && self.in_trim(j))
    }

    /// `i` か、その後ろ (無ければ前) で最も近い削除されていないフレーム。
    fn nearest_kept(&self, i: usize) -> usize {
        let i = i.min(self.len() - 1);
        (i..self.len())
            .find(|&j| self.kept(j))
            .or_else(|| (0..i).rev().find(|&j| self.kept(j)))
            .unwrap_or(i)
    }

    /// クリック: その位置へ。範囲は外す。
    pub fn seek(&mut self, i: usize) {
        self.cur = self.nearest_kept(i);
        self.sel = None;
        self.anchor = None;
    }

    /// 帯のクリック: その 1 枚を選ぶ (1 枚だけ囲んだ矩形と同じ状態)。
    pub fn pick(&mut self, i: usize) {
        self.seek(i);
        self.sel = Some((self.cur, self.cur));
        self.anchor = Some(self.cur);
    }

    /// Shift+クリック: 起点 (最初の Shift+クリックの前の今のフレーム) から `i` までを選ぶ。
    pub fn select_to(&mut self, i: usize) {
        let i = i.min(self.len() - 1);
        let a = *self.anchor.get_or_insert(self.cur);
        self.sel = Some((a.min(i), a.max(i)));
        self.cur = self.nearest_kept(i);
    }

    /// ← → : 削除されていない隣のフレームへ。範囲は外さない。1 枚だけ選んでいれば選択も一緒に動く。
    pub fn step(&mut self, forward: bool) {
        let next = if forward {
            (self.cur + 1..self.len()).find(|&j| self.kept(j))
        } else {
            (0..self.cur).rev().find(|&j| self.kept(j))
        };
        if let Some(j) = next {
            if self.sel == Some((self.cur, self.cur)) {
                self.sel = Some((j, j));
                self.anchor = Some(j);
            }
            self.cur = j;
        }
    }

    /// 範囲 (無ければ今のフレーム) を削除する。トリムの中が空になる削除はしない。
    /// 今のフレームは削除した区間の後ろの最初の残るフレーム (無ければ前) へ移す。
    pub fn delete(&mut self) -> bool {
        let (a, b) = self.sel.unwrap_or((self.cur, self.cur));
        let left = (self.trim.0..=self.trim.1).filter(|&j| self.kept(j) && (j < a || j > b));
        if left.count() == 0 {
            return false;
        }
        let snap = self.snap();
        self.undo.push(snap);
        self.redo.clear();
        self.deleted.extend(a..=b);
        self.cur = self.nearest_kept(b + 1);
        if !self.kept(self.cur) || self.cur < a {
            self.cur = (0..a).rev().find(|&j| self.kept(j)).unwrap_or(self.cur);
        }
        self.sel = None;
        self.anchor = None;
        true
    }

    fn snap(&self) -> Snap {
        (self.deleted.clone(), self.cur, self.sel)
    }

    /// 削除を元に戻す (`redo` なら やり直す)。戻す物が無ければ false。
    pub fn undo_redo(&mut self, redo: bool) -> bool {
        let Some(to) = (if redo {
            self.redo.pop()
        } else {
            self.undo.pop()
        }) else {
            return false;
        };
        let now = self.snap();
        if redo {
            self.undo.push(now);
        } else {
            self.redo.push(now);
        }
        (self.deleted, self.cur, self.sel) = to;
        self.anchor = self.sel.map(|(a, _)| a);
        true
    }

    /// トリムの始め。削除した位置なら後ろの残るフレームへ寄せる。トリムの中に無ければ変えない。
    pub fn set_trim_start(&mut self, i: usize) {
        if let Some(j) = (i.min(self.trim.1)..=self.trim.1).find(|&j| self.kept(j)) {
            self.trim.0 = j;
        }
    }

    /// トリムの終わり。削除した位置なら前の残るフレームへ寄せる。トリムの中に無ければ変えない。
    pub fn set_trim_end(&mut self, i: usize) {
        let i = i.clamp(self.trim.0, self.len() - 1);
        if let Some(j) = (self.trim.0..=i).rev().find(|&j| self.kept(j)) {
            self.trim.1 = j;
        }
    }

    pub fn in_trim(&self, i: usize) -> bool {
        i >= self.trim.0 && i <= self.trim.1
    }

    /// 書き出しの指定。`range` は終わりを含まない。倍率は等倍 (呼び手が `scale_pct` を替える)。
    pub fn options(&self, decimate_fps: Option<u32>) -> GifOptions {
        GifOptions {
            range: self.trim.0..self.trim.1 + 1,
            deleted: self
                .deleted
                .range(self.trim.0..=self.trim.1)
                .copied()
                .collect(),
            decimate_fps,
            end: self.end,
            scale_pct: 100,
            annots: Vec::new(),
        }
    }

    /// フレーム `b` の表示の終わり: 次のフレームの時刻。最後なら録画を止めた時刻 (`clip::encode_gif_within` と同じ)。
    fn end_of(&self, b: usize) -> Duration {
        if b + 1 < self.len() {
            self.ts[b + 1]
        } else {
            self.end
        }
    }

    /// フレーム `i` の自分の時間: 次のフレームの時刻まで (`clip::plan_frames` と同じ規則)。
    pub fn own(&self, i: usize) -> Duration {
        self.end_of(i).saturating_sub(self.ts[i])
    }

    /// `a..=b` の残るフレームの枚数と、その自分の時間の合計。削除したフレームの時間は詰める
    /// (書き出す GIF もその分短い)。
    fn span(&self, a: usize, b: usize) -> (usize, Duration) {
        (a..=b)
            .filter(|&j| self.kept(j))
            .fold((0, Duration::ZERO), |(n, d), j| (n + 1, d + self.own(j)))
    }

    /// 状態の文の枚数と時間: 範囲を選んでいれば範囲の、ほかは書き出す分。
    pub fn status(&self) -> (usize, Duration) {
        match self.sel {
            Some((a, b)) => self.span(a, b),
            None => self.export_span(),
        }
    }

    /// 書き出す分 (トリムの中の残るフレーム)。
    pub fn export_span(&self) -> (usize, Duration) {
        self.span(self.trim.0, self.trim.1)
    }

    /// 録画の全体の時間。
    pub fn total(&self) -> Duration {
        self.end_of(self.len() - 1).saturating_sub(self.ts[0])
    }

    /// 再生: 今のフレームの次に出す、トリムの中の残るフレーム。終わりまで来たらトリムの始めへ戻る。
    pub fn next_play(&self, i: usize) -> usize {
        (i + 1..=self.trim.1)
            .find(|&j| self.kept(j))
            .or_else(|| (self.trim.0..=self.trim.1).find(|&j| self.kept(j)))
            .expect("トリムの中には残るフレームが常にある (delete と set_trim_* が守る)")
    }

    /// `i` を時間 `d` だけ進めた先のフレームと、そのフレームで使い残した時間 (次の刻みに足す)。
    /// 各フレームは自分の時間だけ出す。削除したフレームの時間は飛ばす (書き出す GIF と同じ)。
    pub fn advance(&self, mut i: usize, d: Duration) -> (usize, Duration) {
        if !self.in_trim(i) || !self.kept(i) {
            return (self.next_play(self.trim.1), Duration::ZERO);
        }
        let mut left = d;
        loop {
            let next = self.next_play(i);
            let wraps = next <= i;
            let step = self.own(i);
            if step > left {
                return (i, left);
            }
            if wraps {
                // 一周した。トリムの始めで止める (1 刻みで 2 周はしない)。
                return (next, Duration::ZERO);
            }
            left -= step;
            i = next;
        }
    }
}

/// 時計の書き方 m:ss (切り上げ)。録画バーの残りと経過。
pub fn clock(d: Duration) -> String {
    let s = d.as_secs() + u64::from(d.subsec_nanos() > 0);
    format!("{}:{:02}", s / 60, s % 60)
}

/// 帯の時間 m:ss.cc。
pub fn clock_cs(d: Duration) -> String {
    let cs = d.as_millis() / 10;
    format!("{}:{:02}.{:02}", cs / 6000, cs / 100 % 60, cs % 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    /// 30 fps ちょうど (33.333 ms 間隔) の n 枚。
    fn line(n: usize) -> Timeline {
        Timeline::new(
            (0..n)
                .map(|i| Duration::from_nanos(i as u64 * 33_333_333))
                .collect(),
            Duration::from_nanos(n as u64 * 33_333_333),
        )
    }

    #[test]
    fn options_from_timeline() {
        let mut t = line(1800);
        // 初期: 全体を書き出す。
        let o = t.options(None);
        assert_eq!(o.range, 0..1800);
        assert!(o.deleted.is_empty());
        // DS の見本: トリム 91–1620 (1 始まり) = 添字 90..=1619、削除 180–240 (添字 179..=239)。
        t.set_trim_start(90);
        t.set_trim_end(1619);
        t.seek(179);
        t.select_to(239);
        assert_eq!(t.sel, Some((179, 239)));
        assert!(t.delete());
        let o = t.options(Some(15));
        assert_eq!(o.range, 90..1620);
        assert_eq!(o.deleted.len(), 61);
        assert_eq!(o.deleted.first(), Some(&179));
        assert_eq!(o.deleted.last(), Some(&239));
        assert_eq!(o.decimate_fps, Some(15));
        // トリムの外の削除は渡さない。
        t.seek(5);
        assert!(t.delete());
        assert_eq!(t.options(None).deleted.len(), 61);
    }

    #[test]
    fn undo_redo_deletion() {
        let mut t = line(10);
        t.pick(3);
        t.select_to(5);
        assert!(t.delete());
        assert!(t.undo_redo(false));
        assert!(t.deleted.is_empty());
        assert_eq!((t.cur, t.sel), (5, Some((3, 5))));
        assert!(!t.undo_redo(false));
        assert!(t.undo_redo(true));
        assert_eq!(t.deleted.len(), 3);
        // 新しい削除の後はやり直しが消える。
        t.undo_redo(false);
        t.pick(8);
        assert!(t.delete());
        assert!(!t.undo_redo(true));
    }

    #[test]
    fn pick_matches_a_one_frame_marquee() {
        // 帯のクリックと、1 枚だけを囲んだ矩形 (押下で pick、動かして同じ 1 枚へ select_to) は同じ状態。
        let mut click = line(10);
        click.pick(4);
        let mut marquee = line(10);
        marquee.pick(4);
        marquee.select_to(4);
        assert_eq!((click.cur, click.sel), (marquee.cur, marquee.sel));
        assert_eq!(click.sel, Some((4, 4)));
        // 1 枚の選択は → で一緒に動く。
        click.step(true);
        assert_eq!((click.cur, click.sel), (5, Some((5, 5))));
    }

    #[test]
    fn delete_moves_the_current_frame() {
        let mut t = line(10);
        // 範囲 3..=5 を消すと、今のフレームは後ろの 6 へ。
        t.seek(3);
        t.select_to(5);
        assert!(t.delete());
        assert_eq!(t.cur, 6);
        assert_eq!(t.sel, None);
        // 末尾 8..=9 を消すと、後ろが無いので前の 7 へ。
        t.seek(8);
        t.select_to(9);
        assert!(t.delete());
        assert_eq!(t.cur, 7);
        // 削除した所へのシークは後ろの残るフレームへ寄せる。
        t.seek(4);
        assert_eq!(t.cur, 6);
        // ← は削除を飛ばす。
        t.step(false);
        assert_eq!(t.cur, 2);
        t.step(true);
        assert_eq!(t.cur, 6);
        // 範囲が無ければ今のフレームだけ消す。
        t.seek(6);
        assert!(t.delete());
        assert_eq!(t.cur, 7);
        // 全部が消える削除はしない。
        t.seek(0);
        t.select_to(9);
        assert!(!t.delete());
        assert_eq!(t.deleted.len(), 6);
    }

    #[test]
    fn status_counts_and_seconds() {
        let mut t = line(1800);
        // 通常: 全体 1800 フレーム · 60.00 秒。
        let (n, d) = t.status();
        assert_eq!(n, 1800);
        assert_eq!(format!("{:.2}", d.as_secs_f64()), "60.00");
        // 範囲選択 180–240 (1 始まり): 61 フレーム · 2.03 秒。
        t.seek(179);
        t.select_to(239);
        let (n, d) = t.status();
        assert_eq!(n, 61);
        assert_eq!(format!("{:.2}", d.as_secs_f64()), "2.03");
        // 削除とトリム: 91–1620 の 1530 枚から 61 枚を引いた 1469 枚。
        // 削除したフレームの時間は詰める: 1469 / 30 = 48.97 秒 (DS の GifPin の見本と同じ)。
        assert!(t.delete());
        t.set_trim_start(90);
        t.set_trim_end(1619);
        let (n, d) = t.status();
        assert_eq!(n, 1469);
        assert_eq!(format!("{:.2}", d.as_secs_f64()), "48.97");
        assert_eq!(t.export_span().0, 1469);
    }

    #[test]
    fn playback_stays_in_trim() {
        let mut t = line(10);
        t.set_trim_start(2);
        t.set_trim_end(5);
        t.deleted.insert(4);
        assert_eq!(t.next_play(3), 5);
        assert_eq!(t.next_play(5), 2);
        // 削除した 4 の時間は飛ばす: 2 と 3 が 33.3 ms ずつ出て、5 は 66.7 ms から。
        assert_eq!(t.advance(2, ms(60)).0, 3);
        assert_eq!(t.advance(2, ms(70)).0, 5);
        // 5 は自分の時間 (33.3 ms) だけ出す: 99 ms 進めた残りは 32.3 ms 前後で、まだ 2 へ戻らない。
        let (i, left) = t.advance(2, ms(99));
        assert_eq!(i, 5);
        assert!(left > ms(32) && left < ms(33), "{left:?}");
        // トリムの中を一周するのは 3 枚 × 33.3 ms = 100 ms。
        assert_eq!(t.advance(2, ms(101)), (2, Duration::ZERO));
        // トリムの外からはトリムの始めへ。
        assert_eq!(t.advance(8, ms(10)).0, 2);
    }

    #[test]
    fn still_tail_lasts_until_stop() {
        // 30 fps で 240 枚 (0.0〜7.97 s) のあと画面が止まり、10.0 s で録画を止めた。
        // 最後のフレーム (7.97 s) は止めた時刻まで出る: 全体 10.0 s、最後の 1 枚が 2.03 s。
        let ts: Vec<Duration> = (0..240)
            .map(|i| Duration::from_nanos(i as u64 * 33_333_333))
            .collect();
        let t = Timeline::new(ts.clone(), ms(10_000));
        let total = t.total();
        assert!(total.abs_diff(ms(10_000)) <= ms(34), "{total:?}");
        assert_eq!(t.export_span().1, total);
        // 止めた時刻に複製を足した旧い形の列でも 2 倍にならない (複製の自分の時間は 0)。
        let mut dup = ts;
        dup.push(ms(10_000));
        let total = Timeline::new(dup, ms(10_000)).total();
        assert!(total.abs_diff(ms(10_000)) <= ms(34), "{total:?}");
    }

    #[test]
    fn delete_never_empties_the_trim() {
        let mut t = line(10);
        t.set_trim_start(2);
        t.set_trim_end(5);
        t.seek(2);
        t.select_to(5);
        assert!(!t.delete());
        assert!(t.deleted.is_empty());
        // トリムの外に残るフレームがあっても、トリムの中の最後の 1 枚は消せない。
        t.seek(2);
        t.select_to(4);
        assert!(t.delete());
        t.seek(5);
        assert!(!t.delete());
        assert_eq!(t.export_span().0, 1);
    }

    #[test]
    fn trim_brackets_keep_a_frame_inside() {
        let mut t = line(10);
        t.set_trim_start(2);
        t.set_trim_end(5);
        t.deleted.extend(2..=4);
        // 終わりを削除した区間へ動かしても、中に残るフレームが無いので変えない。
        t.set_trim_end(3);
        assert_eq!(t.trim, (2, 5));
        // 始めを削除した位置へ動かすと、後ろの残るフレームへ寄せる。
        t.set_trim_start(3);
        assert_eq!(t.trim, (5, 5));
        // 広げた終わりが削除した位置なら、前の残るフレームへ寄せる。
        t.deleted.insert(8);
        t.set_trim_end(8);
        assert_eq!(t.trim, (5, 7));
        // 始めを削除した位置へ広げると、後ろの残るフレームへ寄せる。
        t.set_trim_start(2);
        assert_eq!(t.trim, (5, 7));
        t.set_trim_start(0);
        assert_eq!(t.trim, (0, 7));
        // 再生は削除したフレームに落ちない。
        let mut i = 0;
        for _ in 0..20 {
            i = t.next_play(i);
            assert!(t.kept(i) && t.in_trim(i), "{i}");
        }
        // 削除済みのトリムの終わりからも残るフレームへ。
        let mut t = line(10);
        t.set_trim_start(2);
        t.set_trim_end(5);
        t.deleted.insert(5);
        assert_eq!(t.next_play(5), 2);
        assert_eq!(t.advance(8, ms(10)).0, 2);
    }

    #[test]
    fn clocks() {
        assert_eq!(clock(ms(48_000)), "0:48");
        assert_eq!(clock(ms(47_100)), "0:48");
        assert_eq!(clock(ms(60_000)), "1:00");
        assert_eq!(clock_cs(ms(6_000)), "0:06.00");
        assert_eq!(clock_cs(ms(61_230)), "1:01.23");
    }

    #[test]
    fn effective_interval_skips_deleted_and_trim() {
        let mut t = line(30);
        let all: Vec<usize> = t.effective((10, 20)).collect();
        assert_eq!(all, (10..=20).collect::<Vec<_>>());
        // 区間の中の 12–13 を削除し、トリムを 0..=18 にすると、残るのは 10・11・14–18。
        t.seek(12);
        t.select_to(13);
        assert!(t.delete());
        t.set_trim_end(18);
        let got: Vec<usize> = t.effective((10, 20)).collect();
        assert_eq!(got, vec![10, 11, 14, 15, 16, 17, 18]);
        // 区間はフレーム数を超えても切り詰める。
        assert_eq!(t.effective((25, 99)).count(), 0);
        t.set_trim_end(29);
        assert_eq!(t.effective((25, 99)).count(), 5);
    }

    #[test]
    fn annotation_vanishes_when_interval_has_no_kept_frame_and_returns_on_undo() {
        let mut t = line(30);
        // 区間内のフレームを全部削除すると消え、削除の undo で戻る。
        t.seek(10);
        t.select_to(20);
        assert!(t.delete());
        assert_eq!(t.effective((10, 20)).count(), 0);
        assert!(t.undo_redo(false));
        assert_eq!(t.effective((10, 20)).count(), 11);
        // トリムを区間の外へ縮めると消え、トリムを戻すと戻る。
        t.set_trim_end(5);
        assert_eq!(t.effective((10, 20)).count(), 0);
        t.set_trim_end(29);
        assert_eq!(t.effective((10, 20)).count(), 11);
        t.set_trim_start(21);
        assert_eq!(t.effective((10, 20)).count(), 0);
        t.set_trim_start(0);
        assert_eq!(t.effective((10, 20)).count(), 11);
    }
}
