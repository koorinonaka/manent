//! タスクバーのボタンの右クリック (ジャンプリスト) の「タスク」: 静止画キャプチャと動画キャプチャ。
//! 項目は `manent.exe <引数>` を起こし、2 本目のプロセスが 1 本目へ渡す (`main.rs` の `WM_COPYDATA`)。
//! 引数なしの 2 本目 (exe をもう一度開いた) は設定を開かせる。アプリ名の項目は pin::prevent_pinning で出さない。

use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::Common::{IObjectArray, IObjectCollection};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    DestinationList, EnumerableObjectCollection, ICustomDestinationList, IShellLinkW, SHStrDupW,
    ShellLink,
};
use windows::core::{HSTRING, Interface};

/// 2 本目のプロセスが 1 本目に頼むこと。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {
    Settings,
    Still,
    Video,
}

impl Task {
    pub const ALL: [Task; 3] = [Task::Settings, Task::Still, Task::Video];

    /// ジャンプリストの項目の引数。設定は引数なしの起動が開くので持たない。
    fn arg(self) -> Option<&'static str> {
        match self {
            Task::Settings => None,
            Task::Still => Some("--still"),
            Task::Video => Some("--video"),
        }
    }
}

/// 起動引数の頼みごと。引数が無ければ `None` (2 本目なら設定を開かせる)。
pub fn task_arg(args: &[std::ffi::OsString]) -> Option<Task> {
    let first = args.first()?;
    Task::ALL
        .into_iter()
        .find(|t| t.arg().is_some_and(|a| first == a))
}

/// ジャンプリストの「タスク」を今の言語で登録し直す。起動と言語の切り替えで呼ぶ。
pub fn update() {
    match build() {
        Ok(()) => eprintln!("jumplist: updated"),
        Err(e) => eprintln!("jumplist: update failed: {e}"),
    }
}

fn build() -> windows::core::Result<()> {
    // PKEY_Title。ジャンプリストの項目の文言。
    const TITLE: PROPERTYKEY = PROPERTYKEY {
        fmtid: windows::core::GUID::from_u128(0xf29f85e0_4ff9_1068_ab91_08002b27b3d9),
        pid: 2,
    };
    let exe = HSTRING::from(std::env::current_exe().unwrap_or_default().as_os_str());
    let t = crate::strings();
    unsafe {
        let list: ICustomDestinationList =
            CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)?;
        let mut max = 0u32;
        let _removed: IObjectArray = list.BeginList(&mut max)?;
        let tasks: IObjectCollection =
            CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)?;
        for (task, title) in [(Task::Still, t.hotkey_still), (Task::Video, t.hotkey_video)] {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(&exe)?;
            link.SetArguments(&HSTRING::from(task.arg().unwrap_or_default()))?;
            link.SetIconLocation(&exe, 0)?;
            // VT_LPWSTR。文字列は CoTaskMem (SHStrDupW) で持ち、PROPVARIANT の Drop が解く。
            let mut value = PROPVARIANT::default();
            let inner = &mut *value.Anonymous.Anonymous;
            inner.vt = VT_LPWSTR;
            inner.Anonymous.pwszVal = SHStrDupW(&HSTRING::from(title))?;
            let store: IPropertyStore = link.cast()?;
            store.SetValue(&TITLE, &value)?;
            store.Commit()?;
            tasks.AddObject(&link)?;
        }
        list.AddUserTasks(&tasks.cast::<IObjectArray>()?)?;
        list.CommitList()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn task_arg_reads_what_the_list_writes() {
        // 項目に書く引数は、起動の解釈で同じ頼みごとに戻る。
        for t in Task::ALL {
            if let Some(a) = t.arg() {
                assert_eq!(task_arg(&[OsString::from(a)]), Some(t), "{a}");
            }
        }
        let none: [&[&str]; 3] = [&[], &["--open", "--still"], &["--STILL"]];
        for args in none {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            assert_eq!(task_arg(&args), None, "{args:?}");
        }
    }
}
