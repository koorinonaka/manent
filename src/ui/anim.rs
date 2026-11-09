//! 遷移の補間 (ease-out の 3 次と ease-out back)。ツールバーのボタン・範囲選択の吸着の線・設定窓が使う。
//! Windows の「アニメーション効果」(SPI_GETCLIENTAREAANIMATION) がオフなら即座に目標へ行く。
//! 窓ごと出し入れするフェードは DirectComposition に任せる (`Surface::fade`)。

use std::collections::HashMap;
use std::hash::Hash;
use std::time::Instant;

use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};
use windows::core::BOOL;

/// ホバーの出入り。
pub const HOVER_MS: f32 = 120.0;
/// 押下は速く返す。
pub const PRESS_MS: f32 = 70.0;
/// 道具の切り替え (オン・オフ)。
pub const ON_MS: f32 = 160.0;
/// 窓を出す (消すのは DWM の遷移)。
pub const SHOW_MS: f32 = 140.0;
/// ゆっくり消す (GIF ピンのタイムライン)。
pub const FADE_OUT_MS: f32 = 360.0;

/// Windows の「アニメーション効果」。読めなければオン。
pub fn motion() -> bool {
    let mut on = BOOL(1);
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&raw mut on).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    on.as_bool()
}

pub fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Curve {
    /// ease-out (3 次)。
    Out,
    /// ease-out back: 行き過ぎて戻る (最大 約 5%)。
    Back,
}

fn curve(c: Curve, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    match c {
        Curve::Out => ease_out(t),
        Curve::Back => {
            let c1 = 1.2;
            let u = t - 1.0;
            1.0 + (c1 + 1.0) * u.powi(3) + c1 * u.powi(2)
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Tween {
    from: f32,
    to: f32,
    start: Instant,
    ms: f32,
    curve: Curve,
}

impl Tween {
    fn progress(&self, now: Instant) -> f32 {
        if self.ms <= 0.0 {
            return 1.0;
        }
        now.duration_since(self.start).as_secs_f32() * 1000.0 / self.ms
    }

    fn at(&self, now: Instant) -> f32 {
        self.from + (self.to - self.from) * curve(self.curve, self.progress(now))
    }
}

/// 鍵ごとの遷移中の値。描くたびに目標を渡し、今の値を受け取る。
pub struct Anim<K> {
    map: HashMap<K, Tween>,
}

impl<K> Default for Anim<K> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash + Copy> Anim<K> {
    /// `k` を `target` へ `ms` で寄せ、今の値を返す。初めて見る鍵は `target` から始める (出た瞬間は動かさない)。
    /// 目標が変わったら今の値から遷移し直す。
    pub fn to(&mut self, k: K, target: f32, ms: f32, motion: bool) -> f32 {
        self.enter(k, target, ms, motion, target)
    }

    /// `to` と同じだが、初めて見る鍵は `init` から始める (出てくる物のフェード)。
    pub fn enter(&mut self, k: K, target: f32, ms: f32, motion: bool, init: f32) -> f32 {
        self.enter_with(k, target, ms, motion, init, Curve::Out)
    }

    /// `enter` と同じだが、曲線を選ぶ。
    pub fn enter_with(
        &mut self,
        k: K,
        target: f32,
        ms: f32,
        motion: bool,
        init: f32,
        curve: Curve,
    ) -> f32 {
        let now = Instant::now();
        let ms = if motion { ms } else { 0.0 };
        let t = self.map.entry(k).or_insert(Tween {
            from: init,
            to: target,
            start: now,
            ms,
            curve,
        });
        if t.to != target {
            *t = Tween {
                from: t.at(now),
                to: target,
                start: now,
                ms,
                curve,
            };
        }
        t.at(now)
    }

    /// まだ動いている値がある。
    pub fn busy(&self) -> bool {
        let now = Instant::now();
        self.map
            .values()
            .any(|t| t.from != t.to && t.progress(now) < 1.0)
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// `keep` が偽の鍵を捨てる (消えた物は、次に出たときまたフェードで出す)。
    pub fn retain(&mut self, keep: impl Fn(&K) -> bool) {
        self.map.retain(|k, _| keep(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_key_starts_at_target_and_retarget_moves_from_current() {
        let mut a = Anim::default();
        assert_eq!(a.to(1, 1.0, 1000.0, true), 1.0);
        assert!(!a.busy());
        // 目標を変えた瞬間は前の値のまま、以後は遷移中。
        assert!(a.to(1, 0.0, 1000.0, true) > 0.99);
        assert!(a.busy());
        // アニメーション効果がオフなら即座に目標。
        assert_eq!(a.to(2, 0.0, 1000.0, false), 0.0);
        assert_eq!(a.to(2, 1.0, 1000.0, false), 1.0);
    }

    #[test]
    fn back_curve_overshoots_about_five_percent_and_lands() {
        let ts = (0..=1000).map(|i| i as f32 / 1000.0);
        let back = ts
            .clone()
            .map(|t| curve(Curve::Back, t))
            .fold(0.0, f32::max);
        assert!(back > 1.0 && back < 1.06, "back max {back}");
        assert_eq!(curve(Curve::Back, 1.0), 1.0);
        let out = ts.map(|t| curve(Curve::Out, t)).fold(0.0, f32::max);
        assert!(out <= 1.0, "out max {out}");
    }
}
