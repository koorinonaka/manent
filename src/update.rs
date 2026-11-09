//! 更新の確認: GitHub の最新の Release を調べ、今より新しければトレイの窓へ知らせる。
//! HTTP と JSON は Windows の `Windows.Web.Http` / `Windows.Data.Json` (crate を足さない)。
//! 取得はワーカースレッドで行い、結果を `WM_UPDATE` で投函する。

use std::path::PathBuf;
use std::sync::Mutex;

use windows::Data::Json::JsonObject;
use windows::Foundation::Uri;
use windows::Storage::Streams::DataReader;
use windows::Web::Http::Filters::{HttpBaseProtocolFilter, HttpCacheReadBehavior};
use windows::Web::Http::{HttpClient, HttpResponseMessage};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::HSTRING;

/// 取得先の上書き (検証用。別の公開リポジトリやローカルの JSON に向ける)。release でも効かせる:
/// インストーラ版の経路は release でしか確かめられないため。環境変数を書ける者は既にこのプロセスを握っている。
const URL_ENV: &str = "ODILE_UPDATE_URL";

/// 最終 URL (リダイレクトの後) のホストの許可リスト。ここ以外から取ったものは使わない
/// (取ったインストーラをそのまま実行するので、DNS やプロキシで差し替えられた先を踏まない)。
const HOSTS: [&str; 4] = [
    "api.github.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    /// `tag_name` から先頭の `v` を除いたもの。
    pub version: String,
    pub html_url: String,
    /// `Manent-Setup-*.exe` の `browser_download_url`。無ければ None。
    pub installer: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// まだ結果が無い (起動直後・確認中)。
    Unknown,
    Latest,
    Available(Release),
    Failed,
    /// インストーラを落としている。
    Downloading(Release),
}

static STATUS: Mutex<Status> = Mutex::new(Status::Unknown);

pub fn status() -> Status {
    STATUS.lock().expect("update lock").clone()
}

pub fn set_status(s: Status) {
    *STATUS.lock().expect("update lock") = s;
}

/// ワーカーからの知らせ。`WM_UPDATE` の LPARAM に Box で載せる。
pub enum Msg {
    Checked(Result<Release, String>),
    /// 落とした (Ok はそのパス)。呼び手が起動して終了する。
    Downloaded(Result<PathBuf, String>),
}

pub fn take(lparam: LPARAM) -> Box<Msg> {
    unsafe { Box::from_raw(lparam.0 as *mut Msg) }
}

fn post(msg: Msg) {
    let raw = Box::into_raw(Box::new(msg));
    let posted = unsafe {
        PostMessageW(
            Some(crate::tray_hwnd()),
            crate::WM_UPDATE,
            WPARAM(0),
            LPARAM(raw as isize),
        )
    };
    if posted.is_err() {
        drop(unsafe { Box::from_raw(raw) });
    }
}

/// 別スレッドで `latest()` を呼び、結果を投函する。
pub fn check_async() {
    std::thread::spawn(|| {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        post(Msg::Checked(latest()));
    });
}

/// 別スレッドで添付を `%TEMP%` に落とし、結果を投函する。
pub fn download_async(release: Release) {
    std::thread::spawn(move || {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        post(Msg::Downloaded(download(&release)));
    });
}

fn api_url() -> String {
    std::env::var(URL_ENV).unwrap_or_else(|_| {
        let repo = env!("CARGO_PKG_REPOSITORY").trim_end_matches('/');
        let path = repo
            .strip_prefix("https://github.com/")
            .expect("repository is on github.com");
        format!("https://api.github.com/repos/{path}/releases/latest")
    })
}

/// 許可するホストか。上書きの URL のホストも許す (ローカルの JSON で検証するため)。
fn host_allowed(uri: &Uri) -> bool {
    let (Ok(host), Ok(scheme)) = (uri.Host(), uri.SchemeName()) else {
        return false;
    };
    let host = host.to_string().to_ascii_lowercase();
    if scheme == "https" && HOSTS.contains(&host.as_str()) {
        return true;
    }
    std::env::var(URL_ENV)
        .ok()
        .and_then(|u| Uri::CreateUri(&HSTRING::from(u)).ok())
        .and_then(|u| u.Host().ok())
        .is_some_and(|h| h.to_string().eq_ignore_ascii_case(&host))
}

/// `url` を取る。失敗の状態コードと許可リストの外の最終 URL はエラー。
/// WinRT の HTTP キャッシュは読まない: 同じ URL の古い応答 (別のインストーラ) が要求なしで返る (t261109-f7c5 で実測)。
fn get(url: &str) -> Result<HttpResponseMessage, String> {
    let e = |e: windows::core::Error| e.message();
    let filter = HttpBaseProtocolFilter::new().map_err(e)?;
    filter
        .CacheControl()
        .and_then(|c| c.SetReadBehavior(HttpCacheReadBehavior::NoCache))
        .map_err(e)?;
    let client = HttpClient::Create(&filter).map_err(e)?;
    let agent = format!("Manent/{}", env!("CARGO_PKG_VERSION"));
    client
        .DefaultRequestHeaders()
        .and_then(|h| h.UserAgent())
        .and_then(|u| u.TryParseAdd(&HSTRING::from(agent)))
        .map_err(e)?;
    let uri = Uri::CreateUri(&HSTRING::from(url)).map_err(e)?;
    if !host_allowed(&uri) {
        return Err(format!("host not allowed: {url}"));
    }
    let res = client.GetAsync(&uri).and_then(|op| op.join()).map_err(e)?;
    let last = res
        .RequestMessage()
        .and_then(|m| m.RequestUri())
        .map_err(e)?;
    if !host_allowed(&last) {
        return Err(format!(
            "redirected to a host not allowed: {}",
            last.ToString().map_err(e)?
        ));
    }
    let code = res.StatusCode().map_err(e)?.0;
    if !(200..300).contains(&code) {
        return Err(format!("HTTP {code}"));
    }
    Ok(res)
}

/// 最新の Release。ネットワークの失敗も、版の読めない `tag_name` もエラー (「更新なし」にしない)。
pub fn latest() -> Result<Release, String> {
    let url = api_url();
    eprintln!("update: checking {url}");
    let res = get(&url)?;
    let text = res
        .Content()
        .and_then(|c| c.ReadAsStringAsync())
        .and_then(|op| op.join())
        .map_err(|e| e.message())?;
    parse(&text.to_string())
}

/// Release の JSON から版・ページ・インストーラの添付を読む。
fn parse(json: &str) -> Result<Release, String> {
    let e = |e: windows::core::Error| e.message();
    let o = JsonObject::Parse(&HSTRING::from(json)).map_err(e)?;
    let s = |o: &JsonObject, k: &str| {
        o.GetNamedString(&HSTRING::from(k))
            .map(|v| v.to_string())
            .map_err(|err| format!("{k}: {}", err.message()))
    };
    let tag = s(&o, "tag_name")?;
    let version = tag.strip_prefix('v').unwrap_or(&tag).to_string();
    if version_of(&version).is_none() {
        return Err(format!("bad tag_name: {tag}"));
    }
    let assets = o.GetNamedArray(&HSTRING::from("assets")).map_err(e)?;
    let mut installer = None;
    for i in 0..assets.Size().map_err(e)? {
        let a = assets.GetObjectAt(i).map_err(e)?;
        let name = s(&a, "name")?;
        if name.starts_with("Manent-Setup-") && name.ends_with(".exe") {
            installer = Some(s(&a, "browser_download_url")?);
            break;
        }
    }
    Ok(Release {
        version,
        html_url: s(&o, "html_url")?,
        installer,
    })
}

/// `x.y.z` (先頭の `v` は除く) を数値の 3 組に。それ以外は None。
fn version_of(s: &str) -> Option<[u32; 3]> {
    let mut it = s.strip_prefix('v').unwrap_or(s).split('.');
    let v = [
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    ];
    it.next().is_none().then_some(v)
}

/// `tag` が `current` より新しいか。どちらかが読めなければ None。数値で比べる (文字列だと 0.10.0 < 0.9.0)。
pub fn is_newer(tag: &str, current: &str) -> Option<bool> {
    Some(version_of(tag)? > version_of(current)?)
}

/// インストーラ版か: Inno Setup は exe と同じフォルダに `unins000.exe` を置く。
pub fn installed() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("unins000.exe").is_file()))
        .unwrap_or(false)
}

/// 添付を `%TEMP%\Manent-Setup-<版>.exe` に落とす。名前は JSON の `name` を使わず自分で組む。
fn download(release: &Release) -> Result<PathBuf, String> {
    let url = release.installer.as_deref().ok_or("no installer asset")?;
    eprintln!("update: downloading {url}");
    let res = get(url)?;
    let e = |e: windows::core::Error| e.message();
    let buf = res
        .Content()
        .and_then(|c| c.ReadAsBufferAsync())
        .and_then(|op| op.join())
        .map_err(e)?;
    let reader = DataReader::FromBuffer(&buf).map_err(e)?;
    let mut bytes = vec![0; reader.UnconsumedBufferLength().map_err(e)? as usize];
    reader.ReadBytes(&mut bytes).map_err(e)?;
    // 版は `version_of` を通った数字と点だけなので、パスに混ぜてよい。
    let path = std::env::temp_dir().join(format!("Manent-Setup-{}.exe", release.version));
    std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
    eprintln!("update: saved {} ({} bytes)", path.display(), bytes.len());
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_versions() {
        assert_eq!(is_newer("0.10.0", "0.9.0"), Some(true));
        assert_eq!(is_newer("v0.9.1", "0.9.0"), Some(true));
        assert_eq!(is_newer("v1.0.0", "0.9.9"), Some(true));
        assert_eq!(is_newer("v0.9.0", "0.9.0"), Some(false));
        assert_eq!(is_newer("0.8.12", "0.9.0"), Some(false));
        for bad in ["", "v", "0.9", "0.9.0.1", "0.9.0-beta", "x.y.z", "0..1"] {
            assert_eq!(is_newer(bad, "0.9.0"), None, "{bad:?}");
        }
    }

    #[test]
    fn parse_release() {
        let json = r#"{"tag_name":"v0.9.1","html_url":"https://github.com/o/r/releases/tag/v0.9.1",
            "assets":[{"name":"Manent-0.9.1-portable.zip","browser_download_url":"https://github.com/z"},
                      {"name":"Manent-Setup-0.9.1.exe","browser_download_url":"https://github.com/s"}]}"#;
        assert_eq!(
            parse(json).unwrap(),
            Release {
                version: "0.9.1".into(),
                html_url: "https://github.com/o/r/releases/tag/v0.9.1".into(),
                installer: Some("https://github.com/s".into()),
            }
        );
        // インストーラの添付が無ければ None (ポータブルの zip を取り違えない)。
        let no_setup = json.replace("Manent-Setup-0.9.1.exe", "notes.txt");
        assert_eq!(parse(&no_setup).unwrap().installer, None);
        // 読めない tag はエラー (最新版扱いにしない)。
        assert!(parse(&json.replace("v0.9.1\",\"html", "nightly\",\"html")).is_err());
    }
}
