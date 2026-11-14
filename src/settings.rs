//! 設定の全キー。後続タスクはこの型を変えずに値を使う。
//! 欠けたキーは既定値、未知のキーは無視する。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// ホットキーは `hotkey::parse` の文字列表現。空文字列は未割当て。
    pub hotkey_still: String,
    pub hotkey_video: String,
    /// PrintScreen (静止画) / Shift+PrintScreen (動画) を LL フックで横取りする。
    /// Win11 の「Print Screen キーで画面キャプチャを開く」が ON のままでも届く。
    pub printscreen_takeover: bool,
    pub autostart: bool,
    /// エクスプローラの画像の右クリック (旧メニュー) に「Manent でピン留め」を出す (HKCU の SystemFileAssociations)。
    pub explorer_menu: bool,
    pub record_fps: u32,
    pub record_limit_secs: u32,
    pub cursor_in_still: bool,
    pub cursor_in_video: bool,
    /// ピン・GIF ピンとそのツールバー、トレイメニューを、このアプリやほかのアプリのキャプチャに写す。
    /// オフなら `WDA_EXCLUDEFROMCAPTURE` で写さない。範囲選択の面と録画のバーは常に写さない。設定窓は常に写る。
    pub show_in_capture: bool,
    /// 範囲を確定してから録画を始めるまでに数える秒数。`COUNTDOWNS` のどれか。0 はカウントダウンなし。
    /// 前の版の `record_countdown = true/false` は 3 / 0 として読む。
    #[serde(alias = "record_countdown", deserialize_with = "countdown")]
    pub record_countdown_secs: u32,
    /// 未設定なら保存ダイアログは「ピクチャ」から開く。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub save_dir: Option<PathBuf>,
    /// `auto` / `ja` / `en`。
    pub language: String,
    /// GIF の書き出しの倍率 (%)。`GIF_SCALES` のどれか。無い値は 100 として読む。
    #[serde(deserialize_with = "gif_scale")]
    pub gif_export_scale: u32,
    /// GIF の書き出しのファイルサイズの上限 (MB = 1,000,000 バイト)。`GIF_LIMITS` のどれか。0 は上限なし。
    /// 無い値は 0 として読む。
    #[serde(deserialize_with = "gif_limit")]
    pub gif_export_limit_mb: u32,
    /// 起動の後と 24 時間ごとに GitHub の最新の Release を調べる。オフなら外部と一切通信しない。
    pub update_check: bool,
}

/// GIF ピンの書き出しの倍率の選択肢 (%)。
pub const GIF_SCALES: [u32; 5] = [100, 75, 50, 33, 25];

/// GIF ピンの書き出しのサイズ上限の選択肢 (MB)。0 は上限なし。
pub const GIF_LIMITS: [u32; 6] = [0, 25, 10, 5, 3, 1];

/// 録画前のカウントダウンの秒数の選択肢。0 はオフ。
pub const COUNTDOWNS: [u32; 6] = [0, 1, 2, 3, 5, 10];

/// 秒数か、前の版のトグル (bool)。一覧に無い秒数は 3 として読む。
fn countdown<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        On(bool),
        Secs(u32),
    }
    Ok(match V::deserialize(d)? {
        V::On(false) => 0,
        V::Secs(v) if COUNTDOWNS.contains(&v) => v,
        _ => 3,
    })
}

fn gif_limit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = u32::deserialize(d)?;
    Ok(if GIF_LIMITS.contains(&v) { v } else { 0 })
}

fn gif_scale<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = u32::deserialize(d)?;
    Ok(if GIF_SCALES.contains(&v) { v } else { 100 })
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey_still: "PrintScreen".into(),
            hotkey_video: "Shift+PrintScreen".into(),
            // 既定の PrintScreen / Shift+PrintScreen をそのまま届けるため既定は ON。
            // 横取りはキーを捨て、ゲームのスクショのキーなどを潰す。嫌なら OFF にし、
            // ms-screenclip の受け口で取る (docs/design.md)。
            printscreen_takeover: true,
            autostart: true,
            explorer_menu: true,
            record_fps: 30,
            record_limit_secs: 60,
            cursor_in_still: false,
            cursor_in_video: true,
            show_in_capture: false,
            record_countdown_secs: 3,
            save_dir: None,
            language: "auto".into(),
            gif_export_scale: 100,
            gif_export_limit_mb: 0,
            update_check: true,
        }
    }
}

pub enum Loaded {
    Ok(Settings),
    /// ファイルが無かった。既定値を書き出した。
    Created(Settings),
    /// 壊れていた。`.bak` に退避して既定値を書き出した。
    Broken {
        settings: Settings,
        error: String,
    },
}

pub fn parse(text: &str) -> Result<Settings, toml::de::Error> {
    toml::from_str(text)
}

pub fn save(path: &Path, settings: &Settings) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string(settings).expect("Settings always serializes");
    // 一時ファイルに書き切ってから置き換える。途中で切れても settings.toml は前の中身のまま。
    let tmp = path.with_extension("toml.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// 無いファイルは既定値で作る。読めない・0 バイト・UTF-8 でない・TOML として壊れたファイルは
/// `.bak` に退避して既定値で作り直す (`Broken`)。`Err` は退避か書き出しの失敗。
pub fn load(path: &Path) -> std::io::Result<Loaded> {
    let parsed = match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let settings = Settings::default();
            save(path, &settings)?;
            return Ok(Loaded::Created(settings));
        }
        Err(e) => Err(e.to_string()),
        // 空の TOML は parse に通って既定値になるが、書き込みが途中で切れた跡なので壊れた扱い。
        Ok(bytes) if bytes.is_empty() => Err("empty file".to_string()),
        Ok(bytes) => String::from_utf8(bytes)
            .map_err(|e| e.to_string())
            .and_then(|text| parse(&text).map_err(|e| e.to_string())),
    };
    match parsed {
        Ok(settings) => Ok(Loaded::Ok(settings)),
        Err(error) => {
            std::fs::rename(path, path.with_extension("toml.bak"))?;
            let settings = Settings::default();
            save(path, &settings)?;
            Ok(Loaded::Broken { settings, error })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roundtrip() {
        let text = toml::to_string(&Settings::default()).unwrap();
        assert_eq!(parse(&text).unwrap(), Settings::default());
        let with_dir = Settings {
            save_dir: Some(PathBuf::from(r"C:\shots")),
            ..Settings::default()
        };
        assert_eq!(
            parse(&toml::to_string(&with_dir).unwrap()).unwrap(),
            with_dir
        );
    }

    #[test]
    fn missing_and_unknown_keys() {
        let settings = parse("record_fps = 15\nno_such_key = 1\n").unwrap();
        assert_eq!(
            settings,
            Settings {
                record_fps: 15,
                ..Settings::default()
            }
        );
        // カウントダウンは既定 3 秒 (キーの無い古い設定も 3 秒)。前の版のトグルは 3 / 0 秒、一覧に無い値は 3 秒。
        assert_eq!(parse("").unwrap().record_countdown_secs, 3);
        assert_eq!(
            parse(
                "record_countdown = true
"
            )
            .unwrap()
            .record_countdown_secs,
            3
        );
        assert_eq!(
            parse(
                "record_countdown_secs = 4
"
            )
            .unwrap()
            .record_countdown_secs,
            3
        );
        let five = parse(
            "record_countdown_secs = 5
",
        )
        .unwrap();
        assert_eq!(five.record_countdown_secs, 5);
        assert_eq!(parse(&toml::to_string(&five).unwrap()).unwrap(), five);
        // 更新の確認も既定 ON (キーの無い古い設定も ON)。
        assert!(parse("").unwrap().update_check);
        assert!(!parse("update_check = false\n").unwrap().update_check);
        let off = parse("record_countdown = false\n").unwrap();
        assert_eq!(
            off,
            Settings {
                record_countdown_secs: 0,
                ..Settings::default()
            }
        );
        assert_eq!(parse(&toml::to_string(&off).unwrap()).unwrap(), off);
        // GIF の倍率: 5 つのどれかは読み書きで残る。キーが無ければ 100、5 つに無い値も 100。
        assert_eq!(parse("").unwrap().gif_export_scale, 100);
        for v in [100, 75, 50, 33, 25] {
            let s = parse(&format!(
                "gif_export_scale = {v}
"
            ))
            .unwrap();
            assert_eq!(s.gif_export_scale, v);
            assert_eq!(parse(&toml::to_string(&s).unwrap()).unwrap(), s);
        }
        for v in [0, 10, 60, 200] {
            let s = parse(&format!(
                "gif_export_scale = {v}
"
            ))
            .unwrap();
            assert_eq!(s.gif_export_scale, 100, "{v}");
        }
    }

    #[test]
    fn broken_file_is_backed_up() {
        let dir = std::env::temp_dir().join(format!("odile-test-{}", std::process::id()));
        let path = dir.join("settings.toml");
        std::fs::create_dir_all(&dir).unwrap();
        // UTF-16 LE (BOM 付き) の `record_fps = 15`: メモ帳などで保存し直したファイル。
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(
                "record_fps = 15\n"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes),
            )
            .collect();
        let rows: [(&str, &[u8]); 3] = [
            ("toml", b"record_fps = = broken"),
            // 書き込みが途中で切れた跡。
            ("empty", b""),
            ("utf16", &utf16),
        ];
        for (name, bytes) in rows {
            std::fs::write(&path, bytes).unwrap();
            let loaded = load(&path).unwrap();
            assert!(matches!(loaded, Loaded::Broken { .. }), "{name}");
            assert_eq!(
                std::fs::read(dir.join("settings.toml.bak")).unwrap(),
                bytes,
                "{name}"
            );
            assert_eq!(
                parse(&std::fs::read_to_string(&path).unwrap()).unwrap(),
                Settings::default(),
                "{name}"
            );
        }
        // 一時ファイルは残らない。
        assert!(!dir.join("settings.toml.tmp").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
