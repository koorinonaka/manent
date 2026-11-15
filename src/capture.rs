//! 凍結スナップショット: 全モニタを Windows.Graphics.Capture で 1 フレームずつ取り、GPU の texture のまま持つ。
//! 選んだ矩形の切り出し (`crop`) もここで、その矩形だけを CPU の BGRA に読み戻す。座標は仮想画面の物理 px。

use std::time::{Duration, Instant};

use windows::Foundation::Metadata::ApiInformation;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{BOOL, Interface, Result, h};

use crate::ui::gfx;

/// 物理 px の画像 (BGRA、α=255)。
#[derive(Clone)]
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub bgra: Vec<u8>,
}

/// 1 モニタの凍結画像。`rect` は仮想画面の物理 px。`texture` は `gfx()` の device 上の
/// `w`×`h` の BGRA (`D3D11_BIND_SHADER_RESOURCE`)。α は不定 (`crop` が 255 にする)。
pub struct Shot {
    pub monitor: HMONITOR,
    pub rect: RECT,
    pub w: u32,
    pub h: u32,
    pub texture: ID3D11Texture2D,
}

pub fn monitors() -> Vec<(HMONITOR, RECT, RECT)> {
    unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let list = unsafe { &mut *(data.0 as *mut Vec<(HMONITOR, RECT, RECT)>) };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(m, &mut info) }.as_bool() {
            list.push((m, info.rcMonitor, info.rcWork));
        }
        true.into()
    }
    let mut list: Vec<(HMONITOR, RECT, RECT)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&raw mut list as isize));
    }
    list
}

/// 全モニタを凍結する。セッションを全部始めてから、それぞれの最初のフレームを待つ。
pub fn freeze(cursor: bool) -> Result<Vec<Shot>> {
    crate::insight_scope!("capture_freeze");
    let started = Instant::now();
    let (device, interop, border_api, mons) = {
        crate::insight_scope!("freeze_prologue");
        let g = gfx();
        let device: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&g.dxgi)? }.cast()?;
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        // 古い Win10 には IsBorderRequired が無い。
        let border_api = ApiInformation::IsPropertyPresent(
            h!("Windows.Graphics.Capture.GraphicsCaptureSession"),
            h!("IsBorderRequired"),
        )
        .unwrap_or(false);
        (device, interop, border_api, monitors())
    };
    let mut pending = Vec::new();
    for (monitor, rect, _) in mons {
        crate::insight_scope!("freeze_start");
        let (item, size) = {
            crate::insight_scope!("fz_item");
            let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor)? };
            let size = item.Size()?;
            (item, size)
        };
        // HDR のモニタを BGRA8 で撮ると、SDR の白 (scRGB で 1 を超える) が切り詰められて白飛びする。
        // fp16 (scRGB) で撮って、`to_sdr` で SDR の白を 1 にそろえる。
        let white = {
            crate::insight_scope!("fz_white");
            sdr_white(monitor)
        };
        let format = if white.is_some() {
            DirectXPixelFormat::R16G16B16A16Float
        } else {
            DirectXPixelFormat::B8G8R8A8UIntNormalized
        };
        let pool = {
            crate::insight_scope!("fz_pool");
            Direct3D11CaptureFramePool::CreateFreeThreaded(&device, format, 1, size)?
        };
        let session: GraphicsCaptureSession = {
            crate::insight_scope!("fz_session");
            let session: GraphicsCaptureSession = pool.CreateCaptureSession(&item)?;
            let _ = session.SetIsCursorCaptureEnabled(cursor);
            if border_api {
                let _ = session.SetIsBorderRequired(false);
            }
            session
        };
        {
            crate::insight_scope!("fz_start");
            session.StartCapture()?;
        }
        pending.push((monitor, rect, white, pool, session));
    }
    let mut shots = Vec::new();
    let mut whites = Vec::new();
    for (monitor, rect, white, pool, session) in pending {
        let deadline = Instant::now() + Duration::from_secs(2);
        let frame = loop {
            crate::insight_scope!("freeze_wait_poll");
            if let Ok(f) = pool.TryGetNextFrame() {
                break f;
            }
            if Instant::now() > deadline {
                let _ = session.Close();
                let _ = pool.Close();
                return Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_FAIL,
                    "capture: no frame within 2 s",
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        crate::insight_scope!("freeze_read");
        let surface = frame.Surface()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        let source: ID3D11Texture2D = unsafe { access.GetInterface()? };
        // frame pool は `Close` の後に texture を使い回す。自前の texture へ写してから閉じる。
        let copied = match white {
            Some(white) => to_sdr(&source, white),
            None => copy_texture(&source),
        };
        let _ = frame.Close();
        let _ = session.Close();
        let _ = pool.Close();
        let (texture, w, h) = copied?;
        whites.push(white);
        shots.push(Shot {
            monitor,
            rect,
            w,
            h,
            texture,
        });
    }
    eprintln!(
        "capture: froze {} monitor(s) in {:.1} ms (SDR white per monitor, None = SDR: {whites:?})",
        shots.len(),
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(shots)
}

/// モニタが HDR (advanced color) なら、SDR の白の明るさを scRGB の値で返す (1.0 = 80 nit)。
/// SDR なら、またはモニタを DisplayConfig の経路と結び付けられなければ None。
pub(crate) fn sdr_white(monitor: HMONITOR) -> Option<f32> {
    use windows::Win32::Devices::Display::*;
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    unsafe {
        if !GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
            return None;
        }
        let (mut np, mut nm) = (0u32, 0u32);
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm)
            .ok()
            .ok()?;
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        )
        .ok()
        .ok()?;
        for path in &paths[..np as usize] {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0
                || source.viewGdiDeviceName != info.szDevice
            {
                continue;
            }
            let target = |r#type, size| DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type,
                size,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            };
            let mut color = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO {
                header: target(
                    DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                    size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32,
                ),
                ..Default::default()
            };
            // value の bit 1 が advancedColorEnabled。
            if DisplayConfigGetDeviceInfo(&mut color.header) != 0 || color.Anonymous.value & 2 == 0
            {
                return None;
            }
            let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
                header: target(
                    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                    size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                ),
                SDRWhiteLevel: 0,
            };
            if DisplayConfigGetDeviceInfo(&mut white.header) != 0 || white.SDRWhiteLevel == 0 {
                return None;
            }
            // SDRWhiteLevel は 1000 が 80 nit (= scRGB の 1.0)。
            return Some(white.SDRWhiteLevel as f32 / 1000.0);
        }
        None
    }
}

/// fp16 (scRGB、線形) の texture を、SDR の白 `white` を 1 とした BGRA8 (sRGB) の texture にする。
/// `copy_texture` と同じ形 (`USAGE_DEFAULT` + `BIND_SHADER_RESOURCE`) で作る。
fn to_sdr(source: &ID3D11Texture2D, white: f32) -> Result<(ID3D11Texture2D, u32, u32)> {
    crate::insight_scope!("capture_to_sdr");
    let g = gfx();
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        source.GetDesc(&mut desc);
        let (w, h) = (desc.Width, desc.Height);
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        g.d3d
            .CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
        let staging = staging.unwrap();
        g.d3d_ctx.CopyResource(&staging, source);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        g.d3d_ctx
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let lut = sdr_lut(white);
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for r in 0..h as usize {
            let src = std::slice::from_raw_parts(
                (mapped.pData as *const u8).add(r * mapped.RowPitch as usize) as *const u16,
                w as usize * 4,
            );
            let dst = &mut bgra[r * w as usize * 4..(r + 1) * w as usize * 4];
            for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                d[0] = lut[s[2] as usize];
                d[1] = lut[s[1] as usize];
                d[2] = lut[s[0] as usize];
                d[3] = 255;
            }
        }
        g.d3d_ctx.Unmap(&staging, 0);
        let own = D3D11_TEXTURE2D_DESC {
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            ..staging_desc
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: bgra.as_ptr() as *const _,
            SysMemPitch: w * 4,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        g.d3d
            .CreateTexture2D(&own, Some(&data), Some(&mut texture))?;
        Ok((texture.unwrap(), w, h))
    }
}

/// fp16 のビット列 → sRGB の 8 bit。SDR の白 `white` を 255 にする。
/// ponytail: 白より明るい所 (HDR の光) は切り詰める。Snipping Tool と同じ。丸めたいならトーンマップを足す。
pub(crate) fn sdr_lut(white: f32) -> Vec<u8> {
    (0..=u16::MAX)
        .map(|bits| {
            let v = (half(bits) / white).clamp(0.0, 1.0); // NaN は clamp を素通りするので下で 0 になる。
            let s = if v <= 0.0031308 {
                v * 12.92
            } else {
                1.055 * v.powf(1.0 / 2.4) - 0.055
            };
            (s * 255.0).round() as u8
        })
        .collect()
}

/// IEEE 754 半精度のビット列を f32 にする。
fn half(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = ((bits >> 10) & 0x1f) as i32;
    let m = (bits & 0x3ff) as f32;
    sign * match e {
        0 => m * 2f32.powi(-24),
        31 if m == 0.0 => f32::INFINITY,
        31 => f32::NAN,
        _ => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    }
}

/// 写した texture を D2D が bitmap にできる形 (`D3D11_USAGE_DEFAULT` + `BIND_SHADER_RESOURCE`) で作る。
/// 表示は GPU のまま描き (`ui::bitmap_from_texture`)、CPU への読み戻しは `crop` が選んだ矩形だけする。
fn copy_texture(source: &ID3D11Texture2D) -> Result<(ID3D11Texture2D, u32, u32)> {
    let g = gfx();
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        source.GetDesc(&mut desc);
        let own = D3D11_TEXTURE2D_DESC {
            Width: desc.Width,
            Height: desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        g.d3d.CreateTexture2D(&own, None, Some(&mut texture))?;
        let texture = texture.unwrap();
        g.d3d_ctx.CopyResource(&texture, source);
        Ok((texture, desc.Width, desc.Height))
    }
}

/// texture の (`x`, `y`) から `w`×`h` を CPU の BGRA に読み戻す。α はそろえない。
fn read_region(texture: &ID3D11Texture2D, x: u32, y: u32, w: u32, h: u32) -> Result<Image> {
    let g = gfx();
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        g.d3d
            .CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
        let staging = staging.unwrap();
        g.d3d_ctx.CopySubresourceRegion(
            &staging,
            0,
            0,
            0,
            0,
            texture,
            0,
            Some(&D3D11_BOX {
                left: x,
                top: y,
                front: 0,
                right: x + w,
                bottom: y + h,
                back: 1,
            }),
        );
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        g.d3d_ctx
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let row = w as usize * 4;
        let mut bgra = vec![0u8; row * h as usize];
        for r in 0..h as usize {
            let src = std::slice::from_raw_parts(
                (mapped.pData as *const u8).add(r * mapped.RowPitch as usize),
                row,
            );
            bgra[r * row..(r + 1) * row].copy_from_slice(src);
        }
        g.d3d_ctx.Unmap(&staging, 0);
        // α はそろえない (デスクトップの α は不定のことがある)。`paste` が不透明にする。
        Ok(Image { w, h, bgra })
    }
}

/// 凍結画像から仮想画面の矩形を切り出す。モニタの無い所は黒。
/// GPU から読み戻すのは、各モニタと矩形の重なりだけ。
pub fn crop(shots: &[Shot], r: RECT) -> Image {
    crate::insight_scope!("capture_crop");
    let mut parts = Vec::new();
    for shot in shots {
        let m = shot.rect;
        // texture の外 (モニタの矩形と大きさが食い違うとき) は読まない。
        let (l, t) = (r.left.max(m.left), r.top.max(m.top));
        let rr = r.right.min(m.right).min(m.left + shot.w as i32);
        let b = r.bottom.min(m.bottom).min(m.top + shot.h as i32);
        if l >= rr || t >= b {
            continue;
        }
        let (x, y) = ((l - m.left) as u32, (t - m.top) as u32);
        match read_region(&shot.texture, x, y, (rr - l) as u32, (b - t) as u32) {
            Ok(image) => parts.push((
                RECT {
                    left: l,
                    top: t,
                    right: rr,
                    bottom: b,
                },
                image,
            )),
            Err(e) => eprintln!("capture: read failed: {e}"),
        }
    }
    paste(&parts, r)
}

/// 仮想画面の矩形 `r` に、各片 (`(片の矩形, 画像)`) を貼る。片の無い所は黒。結果は不透明。
fn paste(parts: &[(RECT, Image)], r: RECT) -> Image {
    let (w, h) = (
        (r.right - r.left).max(1) as u32,
        (r.bottom - r.top).max(1) as u32,
    );
    let mut out = vec![0u8; (w * h * 4) as usize];
    for (m, image) in parts {
        let (l, t) = (r.left.max(m.left), r.top.max(m.top));
        let (rr, b) = (r.right.min(m.right), r.bottom.min(m.bottom));
        if l >= rr || t >= b {
            continue;
        }
        let iw = image.w as i32;
        for y in t..b {
            let sy = (y - m.top).min(image.h as i32 - 1);
            let sx0 = (l - m.left).min(iw);
            let sx1 = (rr - m.left).min(iw);
            if sx0 >= sx1 {
                continue;
            }
            let src = &image.bgra[((sy * iw + sx0) * 4) as usize..((sy * iw + sx1) * 4) as usize];
            let dy = (y - r.top) as u32;
            let dx = (l - r.left) as u32;
            let at = ((dy * w + dx) * 4) as usize;
            out[at..at + src.len()].copy_from_slice(src);
        }
    }
    // 凍結画像の α は不定。モニタの無い所 (黒) も含めて不透明にする。
    for p in out.chunks_exact_mut(4) {
        p[3] = 255;
    }
    Image { w, h, bgra: out }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 凍結画像の α は不定 (`read_region` はそろえない)。切り出しは必ず不透明で、色は保つ。
    /// GPU の読み戻しは実機でしか動かないので、`crop` の CPU 側 (`paste`) を確かめる。
    #[test]
    fn crop_is_opaque_even_when_the_frozen_alpha_is_not() {
        let part = (
            RECT {
                left: 0,
                top: 0,
                right: 2,
                bottom: 1,
            },
            Image {
                w: 2,
                h: 1,
                bgra: vec![10, 20, 30, 0, 40, 50, 60, 7],
            },
        );
        // 右へ 1 px はみ出す (モニタの無い所は黒)。
        let r = RECT {
            left: 1,
            top: 0,
            right: 3,
            bottom: 1,
        };
        assert_eq!(paste(&[part], r).bgra, [40, 50, 60, 255, 0, 0, 0, 255]);
    }

    /// HDR の SDR 変換: SDR の白 (scRGB で `white`) が 255 になり、半分の線形値は sRGB の 188 になる。
    /// 白より上は 255、負と NaN は 0。白で割り忘れると白飛び、ガンマを忘れると暗くなる。
    #[test]
    fn hdr_sdr_white_maps_to_255() {
        let lut = sdr_lut(2.5);
        // 半精度のビット列: 2.5 = 0x4100、1.25 = 0x3D00、10.0 = 0x4900、-1.0 = 0xBC00、NaN = 0x7E00。
        assert_eq!(lut[0x4100], 255);
        assert_eq!(lut[0x3D00], 188);
        assert_eq!(lut[0x4900], 255);
        assert_eq!(lut[0xBC00], 0);
        assert_eq!(lut[0x7E00], 0);
        assert_eq!(lut[0], 0);
    }
}
