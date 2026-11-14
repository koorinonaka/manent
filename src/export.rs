//! ピンの画像の書き出し: クリップボード (CF_DIB) と PNG 保存 (`IFileSaveDialog` + WIC)。
//! GIF ピンの書き出し: 一時 .gif をクリップボード (CF_HDROP と登録形式 `GIF`) と .gif の保存。

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFOHEADER};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{CF_DIB, CF_HDROP};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::*;
use windows::core::{HSTRING, Result, w};

use crate::capture::Image;

/// 32bpp の下から上の DIB にして載せる (ペイントなどがそのまま貼れる)。
pub fn copy(hwnd: HWND, image: &Image) -> Result<()> {
    put(hwnd, vec![(CF_DIB.0 as u32, dib(image, false))])
}

/// 透明な所のある画像 (束の合成の空き): 透明を保つ PNG (登録形式 `PNG`) と、白に重ねた DIB を載せる
/// (Office・ブラウザなどアルファを読むアプリは先に載せた PNG を取る)。
pub fn copy_clear(hwnd: HWND, image: &Image) -> Result<()> {
    let png = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    put(
        hwnd,
        vec![
            (png, png_bytes(image)?),
            (CF_DIB.0 as u32, dib(image, true)),
        ],
    )
}

/// クリップボードの中身を `formats` (形式, バイト列) に置き換える。
fn put(hwnd: HWND, formats: Vec<(u32, Vec<u8>)>) -> Result<()> {
    unsafe {
        // 先に開く (`set_clipboard` と同じ: 開けなければ何も確保しない)。
        OpenClipboard(Some(hwnd))?;
        let _ = EmptyClipboard();
        let mut result = Ok(());
        for (format, bytes) in &formats {
            let r = global(bytes).and_then(|mem| {
                SetClipboardData(*format, Some(HANDLE(mem.0))).inspect_err(|_| {
                    let _ = GlobalFree(Some(mem));
                })
            });
            if let Err(e) = r {
                result = Err(e);
                break;
            }
        }
        let _ = CloseClipboard();
        result
    }
}

/// `BITMAPINFOHEADER` + 下から上の 32bpp の画素。`white` なら白の上に重ねて不透明にする。
fn dib(image: &Image, white: bool) -> Vec<u8> {
    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: image.w as i32,
        biHeight: image.h as i32,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: image.w * image.h * 4,
        ..Default::default()
    };
    let row = image.w as usize * 4;
    let mut out = Vec::with_capacity(size_of::<BITMAPINFOHEADER>() + row * image.h as usize);
    out.extend_from_slice(unsafe {
        std::slice::from_raw_parts(
            (&raw const header).cast::<u8>(),
            size_of::<BITMAPINFOHEADER>(),
        )
    });
    for y in (0..image.h as usize).rev() {
        let src = &image.bgra[y * row..(y + 1) * row];
        if white {
            for p in src.chunks_exact(4) {
                let a = p[3] as u32;
                let over = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
                out.extend_from_slice(&[over(p[0]), over(p[1]), over(p[2]), 255]);
            }
        } else {
            out.extend_from_slice(src);
        }
    }
    out
}

/// PNG にしたバイト列 (クリップボードの `PNG` 形式)。
fn png_bytes(image: &Image) -> Result<Vec<u8>> {
    use windows::Win32::System::Com::StructuredStorage::{
        CreateStreamOnHGlobal, GetHGlobalFromStream,
    };
    unsafe {
        let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
        encode_png(&stream, image)?;
        let mut stat = Default::default();
        stream.Stat(&mut stat, STATFLAG_NONAME)?;
        let len = stat.cbSize;
        let mem = GetHGlobalFromStream(&stream)?;
        let ptr = GlobalLock(mem) as *const u8;
        let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        let _ = GlobalUnlock(mem);
        Ok(bytes)
    }
}

/// 文字をクリップボードへ (CF_UNICODETEXT)。
pub fn copy_text(hwnd: HWND, text: &str) -> Result<()> {
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
    set_clipboard(hwnd, CF_UNICODETEXT, wide.len() * 2, |ptr| unsafe {
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr.cast::<u16>(), wide.len());
    })
}

/// クリップボードの中身を 1 形式に置き換える。`fill` が `size` バイトを書く。
fn set_clipboard(hwnd: HWND, format: u32, size: usize, fill: impl FnOnce(*mut u8)) -> Result<()> {
    unsafe {
        // 先に開く: 他のプロセスが握っていて失敗したら、何も確保しない (確保してから失敗すると漏れる)。
        OpenClipboard(Some(hwnd))?;
        let mem = match GlobalAlloc(GMEM_MOVEABLE, size) {
            Ok(m) => m,
            Err(e) => {
                let _ = CloseClipboard();
                return Err(e);
            }
        };
        fill(GlobalLock(mem).cast::<u8>());
        let _ = GlobalUnlock(mem);
        let _ = EmptyClipboard();
        let result = SetClipboardData(format, Some(HANDLE(mem.0)));
        let _ = CloseClipboard();
        if result.is_err() {
            let _ = GlobalFree(Some(mem));
        }
        result.map(|_| ())
    }
}

/// 既定のファイル名 (`ext` は拡張子)。
pub fn default_name(ext: &str) -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{} {:04}-{:02}-{:02} {:02}{:02}{:02}.{ext}",
        crate::PRODUCT_NAME,
        t.wYear,
        t.wMonth,
        t.wDay,
        t.wHour,
        t.wMinute,
        t.wSecond
    )
}

/// 保存ダイアログを出し、選ばれたら PNG で書く。取り消しは `Ok(None)`。
/// 初期フォルダは `dir` (設定 `save_dir`)、無ければピクチャ。
pub fn save(hwnd: HWND, image: &Image, dir: Option<&Path>) -> Result<Option<PathBuf>> {
    let Some(path) = ask_path(hwnd, "png", dir)? else {
        return Ok(None);
    };
    write_png(&path, image)?;
    Ok(Some(path))
}

/// 保存ダイアログ (`ext` は `png` か `gif`)。取り消しは `Ok(None)`。
pub fn ask_path(hwnd: HWND, ext: &str, dir: Option<&Path>) -> Result<Option<PathBuf>> {
    let (name, spec) = if ext == "gif" {
        (w!("GIF"), w!("*.gif"))
    } else {
        (w!("PNG"), w!("*.png"))
    };
    unsafe {
        let dialog: IFileSaveDialog =
            CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)?;
        dialog.SetFileTypes(&[COMDLG_FILTERSPEC {
            pszName: name,
            pszSpec: spec,
        }])?;
        dialog.SetDefaultExtension(&HSTRING::from(ext))?;
        dialog.SetFileName(&HSTRING::from(default_name(ext)))?;
        let folder: Option<IShellItem> = match dir {
            Some(d) => SHCreateItemFromParsingName(&HSTRING::from(d.as_os_str()), None).ok(),
            None => SHGetKnownFolderItem(&FOLDERID_Pictures, KF_FLAG_DEFAULT, None).ok(),
        };
        if let Some(f) = folder {
            let _ = dialog.SetFolder(&f);
        }
        if dialog.Show(Some(hwnd)).is_err() {
            return Ok(None);
        }
        let item = dialog.GetResult()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH)?;
        let path = name.to_string();
        CoTaskMemFree(Some(name.0 as _));
        Ok(Some(PathBuf::from(path?)))
    }
}

fn global(bytes: &[u8]) -> Result<HGLOBAL> {
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let ptr = GlobalLock(mem) as *mut u8;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(mem);
        Ok(mem)
    }
}

/// 一時 .gif (`path`、中身は `bytes`) をクリップボードに載せる: ファイル (CF_HDROP) と登録形式 `GIF`。
/// Explorer はファイルとして、チャットはアニメーションのまま貼れる。
pub fn copy_gif(hwnd: HWND, path: &Path, bytes: &[u8]) -> Result<()> {
    let header = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        fWide: true.into(),
        ..Default::default()
    };
    let mut drop = unsafe {
        std::slice::from_raw_parts((&raw const header).cast::<u8>(), size_of::<DROPFILES>())
    }
    .to_vec();
    // ファイル名の並び: 1 つの名前と、終わりの 2 つの NUL。
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();
    drop.extend(wide.iter().flat_map(|c| c.to_le_bytes()));
    unsafe {
        // 先に開く (`set_clipboard` と同じ: 開けなければ何も確保しない)。
        OpenClipboard(Some(hwnd))?;
        let (hdrop, gif) = match (global(&drop), global(bytes)) {
            (Ok(a), Ok(b)) => (a, b),
            (a, b) => {
                for m in [&a, &b].into_iter().flatten() {
                    let _ = GlobalFree(Some(*m));
                }
                let _ = CloseClipboard();
                return Err(a.and(b).unwrap_err());
            }
        };
        let _ = EmptyClipboard();
        let a = SetClipboardData(CF_HDROP.0 as u32, Some(HANDLE(hdrop.0)));
        if a.is_err() {
            let _ = GlobalFree(Some(hdrop));
        }
        let b = SetClipboardData(RegisterClipboardFormatW(w!("GIF")), Some(HANDLE(gif.0)));
        if b.is_err() {
            let _ = GlobalFree(Some(gif));
        }
        let _ = CloseClipboard();
        a.and(b).map(|_| ())
    }
}

pub fn write_png(path: &Path, image: &Image) -> Result<()> {
    let wic = &crate::ui::gfx().wic;
    unsafe {
        let stream = wic.CreateStream()?;
        stream.InitializeFromFilename(
            &HSTRING::from(path.as_os_str()),
            0x4000_0000, /* GENERIC_WRITE */
        )?;
        encode_png(&stream, image)
    }
}

/// `stream` へ PNG (32bpp BGRA) を書く。
fn encode_png(stream: &IStream, image: &Image) -> Result<()> {
    let wic = &crate::ui::gfx().wic;
    unsafe {
        let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        encoder.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
        let frame = frame.unwrap();
        frame.Initialize(None)?;
        frame.SetSize(image.w, image.h)?;
        let mut format = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut format)?;
        frame.WritePixels(image.h, image.w * 4, &image.bgra)?;
        frame.Commit()?;
        encoder.Commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 透明は白、半透明は白に重ね、不透明はそのまま。行は下から上。
    #[test]
    fn dib_flattens_on_white_bottom_up() {
        let image = Image {
            w: 1,
            h: 3,
            bgra: vec![10, 20, 30, 255, 0, 0, 0, 0, 0, 0, 0, 128],
        };
        let px = &dib(&image, true)[size_of::<BITMAPINFOHEADER>()..];
        assert_eq!(
            px,
            [127, 127, 127, 255, 255, 255, 255, 255, 10, 20, 30, 255]
        );
    }
}
