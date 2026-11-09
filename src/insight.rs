//! 処理時間の計測 (`--insight`)。liveback の `insight` を写した物 (tracing は使わない)。
//!
//! `insight` feature が無ければ、このモジュールも `insight_scope!` の呼び場所もコンパイルされない
//! (配布の exe は何も変わらない)。feature があっても `--insight` で起動しなければ何も測らない。
//! 出力は `%LOCALAPPDATA%\Odile\logs\insight.<epoch秒>.log` で、行は 4 種類:
//!
//! - `scope=` 5 秒ごとの区間の集計 (回数・合計・最大・平均)。重い順。
//! - `slow` 1 回が [`SLOW_THRESHOLD`] を越えた瞬間。区間ごとに毎秒 1 行まで。
//! - `heartbeat` 毎秒、このプロセスの CPU とメモリ。
//! - `buffers` 毎秒、Rust のヒープの量 (数える割り当て器)。
//!
//! ファイルへ書くのは報告のスレッドだけ。UI スレッドは行を `PENDING` に積むだけで、ディスクを待たない。

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

/// 60 Hz の 1 コマ (16.6 ms) の半分。これを越えた 1 回は名前を出す。
const SLOW_THRESHOLD: Duration = Duration::from_millis(8);
/// 区間ごとの `slow` の行の間隔の下限。いつも遅い区間が毎コマ書くのを止める。
const SLOW_INTERVAL: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_secs(1);
/// 集計はこの tick ごと。
const FLUSH_TICKS: u32 = 5;
const MB: f64 = 1024.0 * 1024.0;

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
struct Stats {
    count: u64,
    total: Duration,
    max: Duration,
}

/// 区間の集計と `slow` の間引き。ファイルとスレッド無しで数を確かめられるよう分けてある。
/// ponytail: 地図全体に 1 本のロック。区間の閉じは毎秒高々数千で、ロックは測る対象ではない。
#[derive(Default)]
struct Recorder {
    scopes: Mutex<HashMap<&'static str, Stats>>,
    slow_gate: Mutex<HashMap<&'static str, Instant>>,
}

impl Recorder {
    /// 1 回分を足す。`slow` の行を書くべきなら真。
    fn record(&self, name: &'static str, elapsed: Duration, now: Instant) -> bool {
        if let Ok(mut s) = self.scopes.lock() {
            let e = s.entry(name).or_default();
            e.count += 1;
            e.total += elapsed;
            e.max = e.max.max(elapsed);
        }
        if elapsed < SLOW_THRESHOLD {
            return false;
        }
        let Ok(mut gate) = self.slow_gate.lock() else {
            return false;
        };
        match gate.get(name) {
            Some(last) if now.duration_since(*last) < SLOW_INTERVAL => false,
            _ => {
                gate.insert(name, now);
                true
            }
        }
    }

    /// 区間の集計を取り出す。合計の重い順。
    fn drain(&self) -> Vec<(&'static str, Stats)> {
        let Ok(mut s) = self.scopes.lock() else {
            return Vec::new();
        };
        let mut out: Vec<_> = s.drain().collect();
        drop(s);
        out.sort_by(|a, b| b.1.total.cmp(&a.1.total).then(a.0.cmp(b.0)));
        out
    }
}

static ON: AtomicBool = AtomicBool::new(false);
static RECORDER: OnceLock<Recorder> = OnceLock::new();
static STARTED: OnceLock<Instant> = OnceLock::new();
/// 報告のスレッドが書く前の行。
static PENDING: Mutex<Vec<String>> = Mutex::new(Vec::new());
static HEAP_BYTES: AtomicUsize = AtomicUsize::new(0);

fn secs() -> f64 {
    STARTED.get().map_or(0.0, |s| s.elapsed().as_secs_f64())
}

fn emit(line: String) {
    if let Ok(mut p) = PENDING.lock() {
        p.push(line);
    }
}

/// 区間の計測。落ちたときに時間を足す。`insight_scope!` が作る。
pub struct Scope {
    name: &'static str,
    start: Option<Instant>,
}

impl Scope {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            start: ON.load(Ordering::Relaxed).then(Instant::now),
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            add(self.name, start.elapsed());
        }
    }
}

/// 自分で測った 1 回分を `name` の区間に足す (区間で囲めない値: 到着の間隔、刻みの遅れ)。`insight_sample!` が呼ぶ。
pub fn add(name: &'static str, elapsed: Duration) {
    let Some(r) = RECORDER.get() else {
        return;
    };
    if r.record(name, elapsed, Instant::now()) {
        emit(format!(
            "t={:.3} slow scope={name} ms={:.2}",
            secs(),
            elapsed.as_secs_f64() * 1000.0
        ));
    }
}

/// `--insight` で起動していれば、ファイルを作り報告のスレッドを始める。
pub fn start() {
    if !std::env::args_os().any(|a| a == "--insight") {
        return;
    }
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(crate::PRODUCT_NAME)
        .join("logs");
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let path = dir.join(format!("insight.{epoch}.log"));
    let file = std::fs::create_dir_all(&dir).and_then(|_| File::create(&path));
    let mut file = match file {
        Ok(f) => f,
        Err(e) => {
            eprintln!("insight: {} failed: {e}", path.display());
            return;
        }
    };
    let _ = STARTED.set(Instant::now());
    RECORDER.get_or_init(Recorder::default);
    ON.store(true, Ordering::Relaxed);
    eprintln!("insight: writing {}", path.display());
    let cpus = thread::available_parallelism().map_or(1, |n| n.get());
    let _ = writeln!(
        file,
        "t=0.000 start epoch_secs={epoch} profile={} cpus={cpus} slow_ms={} flush_s={}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        SLOW_THRESHOLD.as_millis(),
        TICK.as_secs() * u64::from(FLUSH_TICKS)
    );
    let _ = thread::Builder::new()
        .name("insight-report".into())
        .spawn(move || report_loop(file, cpus));
}

fn report_loop(mut file: File, cpus: usize) {
    let mut last: Option<(Instant, u64)> = None;
    let mut tick: u32 = 0;
    loop {
        thread::sleep(TICK);
        tick = tick.wrapping_add(1);
        let t = secs();
        let mut lines = std::mem::take(&mut *PENDING.lock().unwrap_or_else(|e| e.into_inner()));
        if let Some((cpu, ws, private)) = sample(&mut last, cpus) {
            lines.push(format!(
                "t={t:.3} heartbeat cpu_pct={cpu:.1} working_set_mb={ws:.1} private_mb={private:.1}"
            ));
        }
        lines.push(format!(
            "t={t:.3} buffers rust_heap_mb={:.1}",
            HEAP_BYTES.load(Ordering::Relaxed) as f64 / MB
        ));
        if tick.is_multiple_of(FLUSH_TICKS)
            && let Some(r) = RECORDER.get()
        {
            for (name, s) in r.drain() {
                lines.push(format!(
                    "t={t:.3} scope={name} n={} total_ms={:.2} max_ms={:.2} mean_us={:.1}",
                    s.count,
                    s.total.as_secs_f64() * 1000.0,
                    s.max.as_secs_f64() * 1000.0,
                    s.total.as_secs_f64() * 1e6 / s.count.max(1) as f64
                ));
            }
        }
        for l in lines {
            let _ = writeln!(file, "{l}");
        }
        let _ = file.flush();
    }
}

fn ticks(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

/// (CPU %、working set MB、private MB)。CPU は 2 回目の呼びから (率には 2 点が要る)。
fn sample(last: &mut Option<(Instant, u64)>, cpus: usize) -> Option<(f64, f64, f64)> {
    let process = unsafe { GetCurrentProcess() };
    let (mut c, mut e, mut k, mut u) = Default::default();
    unsafe { GetProcessTimes(process, &mut c, &mut e, &mut k, &mut u).ok()? };
    let busy = ticks(k) + ticks(u);
    let now = Instant::now();
    let prev = last.replace((now, busy));
    let mut m = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    unsafe {
        GetProcessMemoryInfo(
            process,
            (&raw mut m).cast::<PROCESS_MEMORY_COUNTERS>(),
            m.cb,
        )
        .ok()?
    };
    let (t0, b0) = prev?;
    // FILETIME は 100 ns 単位。
    let cpu = (busy - b0) as f64 / 1e7 / now.duration_since(t0).as_secs_f64() / cpus as f64 * 100.0;
    Some((
        cpu,
        m.WorkingSetSize as f64 / MB,
        m.PrivateUsage as f64 / MB,
    ))
}

/// `System` に生きているバイトの合計を足した割り当て器。`insight` feature のときだけ入る。
/// 中で割り当てない (再帰する)。原子的な足し引きだけ。
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            HEAP_BYTES.fetch_add(l.size(), Ordering::Relaxed);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            HEAP_BYTES.fetch_add(l.size(), Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        HEAP_BYTES.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) };
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            HEAP_BYTES.fetch_sub(l.size(), Ordering::Relaxed);
            HEAP_BYTES.fetch_add(new, Ordering::Relaxed);
        }
        q
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_lines_are_rate_limited_per_scope_and_aggregates_sort_heaviest_first() {
        let r = Recorder::default();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        assert!(!r.record("fast", ms(1), t0));
        assert!(r.record("a", ms(9), t0));
        // 同じ区間は 1 秒の間は 2 行目を出さない。別の区間は出す。
        assert!(!r.record("a", ms(20), t0 + ms(500)));
        assert!(r.record("b", ms(9), t0 + ms(500)));
        assert!(r.record("a", ms(9), t0 + ms(1000)));
        let d = r.drain();
        assert_eq!(d[0].0, "a");
        assert_eq!(
            d[0].1,
            Stats {
                count: 3,
                total: ms(38),
                max: ms(20)
            }
        );
        assert_eq!(
            d.iter().map(|x| x.0).collect::<Vec<_>>(),
            ["a", "b", "fast"]
        );
        assert!(r.drain().is_empty());
    }
}
