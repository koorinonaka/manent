//! ストア版 (MSIX) の右クリックの上段メニュー「Manent でピン留め」。
//! Explorer が `desktop4:FileExplorerContextMenus` の Clsid で `DllHost.exe` に読み込ませる。
//! DllHost には package identity があり、ここから起こした `manent.exe` にも同じ identity が付く
//! (root/t261110-1641 の実測。KNOWLEDGE.md「MSIX とストア版」)。
//!
//! 設定は exe と同じ `%APPDATA%\Manent\settings.toml` をそのパスで読む。パッケージの仮想化で
//! LocalCache に入っていても、同じパスで同じ中身が見える (1641 の実測)。
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_NOTIMPL, HMODULE, S_FALSE,
};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx, IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows::Win32::UI::Shell::{
    ECS_ENABLED, ECS_HIDDEN, IEnumExplorerCommand, IExplorerCommand, IExplorerCommand_Impl,
    IShellItemArray, SHStrDupW, SIGDN_FILESYSPATH,
};
use windows::core::{BOOL, GUID, HRESULT, Interface, PCWSTR, PWSTR, Ref, implement};

/// `packaging/AppxManifest.xml` の `com:Class Id` と `desktop5:Verb Clsid` と同じ値。
const CLSID: GUID = GUID::from_u128(0xe33594c0_8ebe_4f5a_a30f_c591a89d1b42);

/// `src/i18n.rs` の `explorer_verb` (`{app}` = `Manent`) の写し。あちらを変えたらここも変える。
const TITLE_JA: &str = "Manent でピン留め";
const TITLE_EN: &str = "Pin with Manent";

static MODULE: AtomicIsize = AtomicIsize::new(0);

/// 設定の中身 (無ければ `None`) から、項目を出すかと、日本語か (`None` は OS の UI 言語で決める) を返す。
/// exe の `src/settings.rs` の `load` と同じく、無い・壊れている・キーが無いときは既定
/// (`explorer_menu = true`・`language = "auto"`) を使う。言語は `src/i18n.rs` の `is_ja` と同じ規則。
fn decide(text: Option<&str>) -> (bool, Option<bool>) {
    let table = text.and_then(|t| t.parse::<toml::Table>().ok());
    let get = |key: &str| table.as_ref().and_then(|t| t.get(key));
    let shown = get("explorer_menu")
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    let ja = match get("language").and_then(toml::Value::as_str) {
        Some("ja") => Some(true),
        Some("en") => Some(false),
        _ => None,
    };
    (shown, ja)
}

fn settings() -> (bool, Option<bool>) {
    let path = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(r"Manent\settings.toml");
    decide(std::fs::read_to_string(path).ok().as_deref())
}

fn os_is_japanese() -> bool {
    const LANG_JAPANESE: u16 = 0x11;
    // 下位 10 bit が primary language。
    unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() & 0x3FF == LANG_JAPANESE }
}

#[unsafe(no_mangle)]
extern "system" fn DllMain(module: HMODULE, reason: u32, _: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(module.0 as isize, Ordering::Relaxed);
    }
    true.into()
}

/// この DLL と同じフォルダの `manent.exe`。
fn exe_path() -> PathBuf {
    let module = HMODULE(MODULE.load(Ordering::Relaxed) as *mut c_void);
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    PathBuf::from(String::from_utf16_lossy(&buf[..n])).with_file_name("manent.exe")
}

fn dup(s: &str) -> windows::core::Result<PWSTR> {
    let w: Vec<u16> = s.encode_utf16().chain(Some(0)).collect();
    unsafe { SHStrDupW(PCWSTR(w.as_ptr())) }
}

fn item_paths(items: &IShellItemArray) -> windows::core::Result<Vec<String>> {
    let mut out = Vec::new();
    for i in 0..unsafe { items.GetCount()? } {
        let item = unsafe { items.GetItemAt(i)? };
        let p = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH)? };
        out.push(unsafe { p.to_string() }.unwrap_or_default());
        unsafe { CoTaskMemFree(Some(p.0 as *const c_void)) };
    }
    Ok(out)
}

#[implement(IExplorerCommand)]
struct PinCommand;

impl IExplorerCommand_Impl for PinCommand_Impl {
    fn GetTitle(&self, _: Ref<IShellItemArray>) -> windows::core::Result<PWSTR> {
        let ja = settings().1.unwrap_or_else(os_is_japanese);
        dup(if ja { TITLE_JA } else { TITLE_EN })
    }
    fn GetIcon(&self, _: Ref<IShellItemArray>) -> windows::core::Result<PWSTR> {
        dup(&format!("{},0", exe_path().display()))
    }
    fn GetToolTip(&self, _: Ref<IShellItemArray>) -> windows::core::Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }
    fn GetCanonicalName(&self) -> windows::core::Result<GUID> {
        Ok(CLSID)
    }
    fn GetState(&self, _: Ref<IShellItemArray>, _: BOOL) -> windows::core::Result<u32> {
        Ok(if settings().0 {
            ECS_ENABLED.0 as u32
        } else {
            ECS_HIDDEN.0 as u32
        })
    }
    /// 項目ごとに `manent.exe --open <path>` を起こす。exe は 1 プロセス 1 パスで受け、
    /// 起動済みのインスタンスへ渡す (`src/main.rs` の `open_arg` と `send_to_first`)。
    fn Invoke(&self, items: Ref<IShellItemArray>, _: Ref<IBindCtx>) -> windows::core::Result<()> {
        let Some(items) = items.as_ref() else {
            return Ok(());
        };
        let exe = exe_path();
        for path in item_paths(items)? {
            // 起こせなくても Explorer へ返すエラーは無い。メニューの項目は何もしないだけ。
            let _ = std::process::Command::new(&exe)
                .arg("--open")
                .arg(path)
                .spawn();
        }
        Ok(())
    }
    fn GetFlags(&self) -> windows::core::Result<u32> {
        Ok(0)
    }
    fn EnumSubCommands(&self) -> windows::core::Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<windows::core::IUnknown>,
        iid: *const GUID,
        object: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        unsafe { *object = std::ptr::null_mut() };
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let cmd: IExplorerCommand = PinCommand.into();
        unsafe { cmd.query(iid, object).ok() }
    }
    fn LockServer(&self, _: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

/// # Safety
/// COM が有効なポインタを渡す。
#[unsafe(no_mangle)]
unsafe extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    iid: *const GUID,
    object: *mut *mut c_void,
) -> HRESULT {
    unsafe {
        *object = std::ptr::null_mut();
        if *clsid != CLSID {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = Factory.into();
        factory.query(iid, object)
    }
}

#[unsafe(no_mangle)]
extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// exe の `settings.rs` の既定と同じに出し分ける。言語は `i18n.rs` の `is_ja` と同じ規則。
    #[test]
    fn decide_table() {
        type Row = (Option<&'static str>, (bool, Option<bool>));
        let rows: &[Row] = &[
            (None, (true, None)),
            (Some("explorer_menu = false\n"), (false, None)),
            (Some("explorer_menu = true\n"), (true, None)),
            (Some("explorer_menu = false\nlanguage = [\n"), (true, None)),
            (Some("record_fps = 15\n"), (true, None)),
            (Some("language = \"ja\"\n"), (true, Some(true))),
            (Some("language = \"en\"\n"), (true, Some(false))),
            (Some("language = \"auto\"\n"), (true, None)),
        ];
        for (text, want) in rows {
            assert_eq!(decide(*text), *want, "{text:?}");
        }
    }
}
