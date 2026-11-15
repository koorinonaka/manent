//! UI 要素の問い合わせ。UI スレッドで UIA を呼ばない (相手が固まると巻き込まれる) ので、
//! 専用スレッド (MTA) で呼び、答えを `PostMessage` で返す。マウスの追従はこの答えを待たない。
//!
//! `ElementFromPoint` は使わない: オーバーレイが最前面を覆っているので、自分の窓が返る。
//! かわりに、オーバーレイが Z 順で見つけた窓を `ElementFromHandle` で開き、点を含む子へ降りる。

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::System::Com::*;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::Result;

use crate::ui::contains;

#[derive(Clone, Debug)]
pub struct Element {
    pub rect: RECT,
    pub kind: String,
    pub name: String,
}

pub struct Answer {
    /// 問い合わせた窓。
    pub hwnd: isize,
    /// 深い順。`chain[0]` が点の下のいちばん深い要素、最後が窓そのもの。
    pub chain: Vec<Element>,
    pub ms: f64,
}

pub struct Request {
    pub hwnd: isize,
    pub pt: POINT,
}

/// 1 回の降下に使う上限。巨大な木 (ブラウザ) で止まらないようにする。
const BUDGET: Duration = Duration::from_millis(400);

/// 答えは `notify` へ `msg` で投函する。lparam は `Box<Answer>` の生ポインタ (`take` で受ける)。
pub fn start(notify: HWND, msg: u32) -> Sender<Request> {
    let (tx, rx) = channel::<Request>();
    let notify = notify.0 as isize;
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let auto: IUIAutomation =
            match unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) } {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("uia: init failed: {e}");
                    return;
                }
            };
        run(&auto, &rx, HWND(notify as *mut _), msg);
    });
    tx
}

fn run(auto: &IUIAutomation, rx: &Receiver<Request>, notify: HWND, msg: u32) {
    while let Ok(mut req) = rx.recv() {
        // 溜まった点は最新だけを使う。待たない (カーソルが動き続けても答えを返し続ける)。
        while let Ok(newer) = rx.try_recv() {
            req = newer;
        }
        let started = Instant::now();
        let chain = query(auto, HWND(req.hwnd as *mut _), req.pt).unwrap_or_default();
        let answer = Box::new(Answer {
            hwnd: req.hwnd,
            chain,
            ms: started.elapsed().as_secs_f64() * 1000.0,
        });
        let raw = Box::into_raw(answer);
        if unsafe { PostMessageW(Some(notify), msg, WPARAM(0), LPARAM(raw as isize)) }.is_err() {
            drop(unsafe { Box::from_raw(raw) });
        }
    }
}

/// `PostMessage` の lparam から答えを取り出す。
pub fn take(lparam: LPARAM) -> Box<Answer> {
    unsafe { Box::from_raw(lparam.0 as *mut Answer) }
}

fn area(r: &RECT) -> i64 {
    (r.right - r.left) as i64 * (r.bottom - r.top) as i64
}

fn element(e: &IUIAutomationElement, cached: bool) -> Option<Element> {
    unsafe {
        let rect = if cached {
            e.CachedBoundingRectangle()
        } else {
            e.CurrentBoundingRectangle()
        }
        .ok()?;
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return None;
        }
        let kind = if cached {
            e.CachedLocalizedControlType()
        } else {
            e.CurrentLocalizedControlType()
        }
        .map(|b| b.to_string())
        .unwrap_or_default();
        let name = if cached {
            e.CachedName()
        } else {
            e.CurrentName()
        }
        .map(|b| b.to_string())
        .unwrap_or_default();
        Some(Element { rect, kind, name })
    }
}

fn query(auto: &IUIAutomation, hwnd: HWND, pt: POINT) -> Result<Vec<Element>> {
    let started = Instant::now();
    unsafe {
        let root = auto.ElementFromHandle(hwnd)?;
        let cache = auto.CreateCacheRequest()?;
        cache.AddProperty(UIA_BoundingRectanglePropertyId)?;
        cache.AddProperty(UIA_NamePropertyId)?;
        cache.AddProperty(UIA_LocalizedControlTypePropertyId)?;
        cache.AddProperty(UIA_ControlTypePropertyId)?;
        let all = auto.CreateTrueCondition()?;
        let mut path = Vec::new();
        if let Some(e) = element(&root, false) {
            path.push(e);
        }
        let Ok(mut children) = root.FindAllBuildCache(TreeScope_Children, &all, &cache) else {
            return Ok(path);
        };
        for _ in 0..40 {
            if started.elapsed() > BUDGET {
                break;
            }
            let n = children.Length().unwrap_or(0);
            // 点を含む子を小さい順に (重なった兄弟では手前の小さい方が当たる)。
            let mut hits: Vec<(i64, IUIAutomationElement, Element)> = Vec::new();
            for i in 0..n {
                let Ok(child) = children.GetElement(i) else {
                    continue;
                };
                let Some(info) = element(&child, true) else {
                    continue;
                };
                if contains(&info.rect, pt) {
                    hits.push((area(&info.rect), child, info));
                }
            }
            hits.sort_by_key(|h| h.0);
            // 名前も子も無いペインは飛ばす。Chromium の `Intermediate D3D Window` や
            // UWP の `ApplicationFrameInputSinkWindow` が中身に重なって置かれ、中身より小さいか同じ大きさで先に当たる。
            let mut next = None;
            for (_, child, info) in hits {
                let Ok(grand) = child.FindAllBuildCache(TreeScope_Children, &all, &cache) else {
                    continue;
                };
                let empty_pane = info.name.is_empty()
                    && child
                        .CachedControlType()
                        .is_ok_and(|t| t == UIA_PaneControlTypeId)
                    && grand.Length().unwrap_or(0) == 0;
                if !empty_pane {
                    next = Some((grand, info));
                    break;
                }
            }
            let Some((grand, info)) = next else { break };
            path.push(info);
            children = grand;
        }
        path.reverse();
        Ok(path)
    }
}
