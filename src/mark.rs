//! トレイの 1 色のマーク。原図 (256 の格子) を実行時に D2D で描いて HICON にする。
//! 色はタスクバーの明暗で選ぶ (`tray-ink-light-taskbar` #111114 / `tray-ink-dark-taskbar` #FFFFFF)。

use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject,
};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, GetSystemMetrics, HICON, ICONINFO, SM_CXSMICON,
};
use windows::core::Result;

use crate::ui::{self, gfx, widgets};

/// 原図は `.agents/tasks/evidence/direct-20261104-icon-balance/icon.py` の `TRAY` (16 の格子を 16 倍)。
/// 枠は外 48〜240・内 80〜208、頭は中心 (48, 48) 半径 48、空きは半径 68。ストロークは幅 28 の折れ線で、
/// アプリのアイコンのジグザグを 1 色にしたもの。入りと終わりは枠の輪に重なる。
const FRAME: &str = "M116 48L224 48A16 16 0 0 1 240 64L240 224A16 16 0 0 1 224 240L64 240A16 16 0 0 1 48 224L48 116A68 68 0 0 0 80 108L80 208L208 208L208 80L108 80A68 68 0 0 0 116 48Z";
const HEAD: &str = "M0 48A48 48 0 1 0 96 48A48 48 0 1 0 0 48Z";
const STROKE: &str = "M216 64L105.6 131.2L179.2 166.4L140.8 224";
const STROKE_WIDTH: f32 = 28.0;

/// トレイのアイコンの大きさ (100% 16、125% 20、150% 24)。`LoadIconMetric(LIM_SMALL)` と同じ
/// `SM_CXSMICON` を使う。
pub fn tray_size() -> u32 {
    unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32
}

/// 辺 `size` のマークの画素 (BGRA、straight alpha)。色はインク 1 色、形は α だけが持つ。
pub fn pixels(size: u32, dark_taskbar: bool) -> Result<Vec<u8>> {
    let ink: u32 = if dark_taskbar { 0xFFFFFF } else { 0x111114 };
    let alpha = render_alpha(size)?;
    Ok(alpha
        .iter()
        .flat_map(|&a| [ink as u8, (ink >> 8) as u8, (ink >> 16) as u8, a])
        .collect())
}

/// タスクバーの明暗に合うインクで、辺 `size` のマークを作る。
pub fn tray_icon(size: u32) -> Result<HICON> {
    let src = pixels(size, ui::taskbar_dark())?;
    unsafe {
        let header = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            biHeight: -(size as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let info = BITMAPINFO {
            bmiHeader: header,
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
        std::slice::from_raw_parts_mut(bits as *mut u8, src.len()).copy_from_slice(&src);
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, None);
        let icon = CreateIconIndirect(&ICONINFO {
            fIcon: true.into(),
            hbmMask: mask,
            hbmColor: color,
            ..Default::default()
        });
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

/// 原図を `size` に縮めて描き、各画素の α (覆う割合) を返す。
fn render_alpha(size: u32) -> Result<Vec<u8>> {
    let g = gfx();
    unsafe {
        let bitmap = g.wic.CreateBitmap(
            size,
            size,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapCacheOnLoad,
        )?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        let rt = g.factory.CreateWicBitmapRenderTarget(&bitmap, &props)?;
        let k = size as f32 / 256.0;
        rt.BeginDraw();
        rt.Clear(Some(&ui::TRANSPARENT));
        rt.SetTransform(&windows_numerics::Matrix3x2::scale(k, k));
        let brush = rt.CreateSolidColorBrush(&ui::hex(0xFFFFFF), None)?;
        for d in [FRAME, HEAD] {
            rt.FillGeometry(&widgets::geometry(d)?, &brush, None);
        }
        rt.DrawGeometry(
            &widgets::geometry(STROKE)?,
            &brush,
            STROKE_WIDTH,
            &widgets::round_stroke(),
        );
        rt.EndDraw(None, None)?;
        let mut buf = vec![0u8; (size * size * 4) as usize];
        bitmap.CopyPixels(std::ptr::null(), size * 4, &mut buf)?;
        Ok(buf.chunks_exact(4).map(|p| p[3]).collect())
    }
}
