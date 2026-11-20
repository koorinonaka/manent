//! ホットキーの文字列表現 (`Shift+PrintScreen`) と `RegisterHotKey` の (mod, vk) の相互変換。
//! 空文字列は未割当て。

pub const MOD_ALT: u32 = 0x1;
pub const MOD_CONTROL: u32 = 0x2;
pub const MOD_SHIFT: u32 = 0x4;
pub const MOD_WIN: u32 = 0x8;

pub const VK_SNAPSHOT: u32 = 0x2C;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: u32,
    pub vk: u32,
}

/// 書き出し順。parse はどの順でも受ける。
const MODS: [(u32, &str); 4] = [
    (MOD_CONTROL, "Ctrl"),
    (MOD_ALT, "Alt"),
    (MOD_SHIFT, "Shift"),
    (MOD_WIN, "Win"),
];

const NAMED: [(u32, &str); 18] = [
    (VK_SNAPSHOT, "PrintScreen"),
    (0x13, "Pause"),
    (0x20, "Space"),
    (0x0D, "Enter"),
    (0x1B, "Escape"),
    (0x09, "Tab"),
    (0x08, "Backspace"),
    (0x2D, "Insert"),
    (0x2E, "Delete"),
    (0x24, "Home"),
    (0x23, "End"),
    (0x21, "PageUp"),
    (0x22, "PageDown"),
    (0x25, "Left"),
    (0x26, "Up"),
    (0x27, "Right"),
    (0x28, "Down"),
    (0x91, "ScrollLock"),
];

fn key_name(vk: u32) -> Option<String> {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => Some(char::from_u32(vk)?.to_string()),
        0x70..=0x87 => Some(format!("F{}", vk - 0x70 + 1)),
        _ => NAMED
            .iter()
            .find(|(v, _)| *v == vk)
            .map(|(_, n)| n.to_string()),
    }
}

fn key_vk(name: &str) -> Option<u32> {
    let upper = name.to_ascii_uppercase();
    let mut chars = upper.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && c.is_ascii_alphanumeric()
    {
        return Some(c as u32);
    }
    if let Some(n) = upper.strip_prefix('F').and_then(|n| n.parse::<u32>().ok())
        && (1..=24).contains(&n)
    {
        return Some(0x70 + n - 1);
    }
    NAMED
        .iter()
        .find(|(_, n)| n.eq_ignore_ascii_case(name))
        .map(|(v, _)| *v)
}

/// `Ok(None)` は未割当て (空文字列)。
pub fn parse(text: &str) -> Result<Option<Hotkey>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let parts: Vec<&str> = text.split('+').map(str::trim).collect();
    let (key, mod_names) = parts.split_last().expect("split never yields zero parts");
    let mut mods = 0;
    for name in mod_names {
        let bit = match name.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => MOD_CONTROL,
            "alt" => MOD_ALT,
            "shift" => MOD_SHIFT,
            "win" => MOD_WIN,
            _ => return Err(format!("unknown modifier `{name}` in `{text}`")),
        };
        mods |= bit;
    }
    let vk = key_vk(key).ok_or_else(|| format!("unknown key `{key}` in `{text}`"))?;
    Ok(Some(Hotkey { mods, vk }))
}

pub fn format(hotkey: Option<Hotkey>) -> String {
    let Some(Hotkey { mods, vk }) = hotkey else {
        return String::new();
    };
    let mut out: Vec<String> = MODS
        .iter()
        .filter(|(bit, _)| mods & bit != 0)
        .map(|(_, n)| n.to_string())
        .collect();
    out.push(key_name(vk).unwrap_or_else(|| format!("0x{vk:02X}")));
    out.join("+")
}

/// ホットキー記録欄の 1 回の記録。キーの押し下げと離しを順に食わせ、押している修飾キーを自分で数える。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Recorder {
    /// いま押している修飾キー (`MOD_*`)。記録中のキーキャップに出す。
    pub mods: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// 確定した。
    Key(Hotkey),
    /// Esc: 取り消し (元の値のまま)。
    Cancel,
    /// Backspace: 未割当てにする。
    Clear,
    /// Delete: 既定のキーに戻す。
    Default,
    /// 修飾キーなしでは割り当てられないキー (`vk`)。記録は続く。
    NeedsModifier(u32),
    /// 名前の無いキー (記号・テンキーなど)。記録は続く。
    Unsupported,
}

fn modifier_bit(vk: u32) -> Option<u32> {
    match vk {
        0x10 | 0xA0 | 0xA1 => Some(MOD_SHIFT),
        0x11 | 0xA2 | 0xA3 => Some(MOD_CONTROL),
        0x12 | 0xA4 | 0xA5 => Some(MOD_ALT),
        0x5B | 0x5C => Some(MOD_WIN),
        _ => None,
    }
}

/// 修飾キーなしで割り当ててよいキー (ほかのアプリの文字入力を奪わないもの)。
fn bare_ok(vk: u32) -> bool {
    matches!(vk, 0x70..=0x87 | VK_SNAPSHOT | 0x13 | 0x91)
}

impl Recorder {
    /// `down` は押し下げ。修飾キーだけなら `None`。名前の無いキー・修飾なしの文字キーは確定せず理由を返す。
    pub fn key(&mut self, vk: u32, down: bool) -> Option<Recorded> {
        if let Some(bit) = modifier_bit(vk) {
            if down {
                self.mods |= bit;
            } else {
                self.mods &= !bit;
            }
            return None;
        }
        if !down {
            return None;
        }
        match (vk, self.mods) {
            (0x1B, 0) => return Some(Recorded::Cancel),
            (0x08, 0) => return Some(Recorded::Clear),
            (0x2E, 0) => return Some(Recorded::Default),
            _ => {}
        }
        if key_name(vk).is_none() {
            return Some(Recorded::Unsupported);
        }
        if self.mods == 0 && !bare_ok(vk) {
            return Some(Recorded::NeedsModifier(vk));
        }
        Some(Recorded::Key(Hotkey {
            mods: self.mods,
            vk,
        }))
    }

    /// キーキャップに出す名前 (書き出し順)。
    pub fn caps(mods: u32) -> Vec<&'static str> {
        MODS.iter()
            .filter(|(bit, _)| mods & bit != 0)
            .map(|(_, n)| *n)
            .collect()
    }
}

/// キーキャップに出す名前の列 (修飾キー + キー)。
pub fn caps_of(hotkey: Hotkey) -> Vec<String> {
    let mut out: Vec<String> = Recorder::caps(hotkey.mods)
        .into_iter()
        .map(String::from)
        .collect();
    out.push(key_name(hotkey.vk).unwrap_or_else(|| format!("0x{:02X}", hotkey.vk)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (vk, down) の列を食わせ、最後に出た結果を返す。
    fn feed(keys: &[(u32, bool)]) -> (Option<Recorded>, u32) {
        let mut r = Recorder::default();
        let mut last = None;
        for &(vk, down) in keys {
            if let Some(out) = r.key(vk, down) {
                last = Some(out);
            }
        }
        (last, r.mods)
    }

    #[test]
    fn recorder_sequences() {
        const CTRL: u32 = 0x11;
        const ALT: u32 = 0x12;
        const SHIFT: u32 = 0x10;
        let d = |vk| (vk, true);
        let u = |vk| (vk, false);
        // Ctrl+Alt+Shift+P で確定する。
        assert_eq!(
            feed(&[d(CTRL), d(ALT), d(SHIFT), d(0x50)]).0,
            Some(Recorded::Key(parse("Ctrl+Alt+Shift+P").unwrap().unwrap()))
        );
        // 修飾キーだけでは確定しない。押している修飾キーは残る。
        assert_eq!(feed(&[d(CTRL), d(SHIFT)]), (None, MOD_CONTROL | MOD_SHIFT));
        // 離した修飾キーは外れる: Ctrl を離してから Shift+S。
        assert_eq!(
            feed(&[d(CTRL), u(CTRL), d(SHIFT), d(0x53)]).0,
            Some(Recorded::Key(parse("Shift+S").unwrap().unwrap()))
        );
        // Esc は取り消し、Backspace は未割当て。修飾キー付きならふつうのキー。
        assert_eq!(feed(&[d(0x1B)]).0, Some(Recorded::Cancel));
        assert_eq!(feed(&[d(0x08)]).0, Some(Recorded::Clear));
        assert_eq!(feed(&[d(0x2E)]).0, Some(Recorded::Default));
        assert_eq!(
            feed(&[d(CTRL), d(0x08)]).0,
            Some(Recorded::Key(parse("Ctrl+Backspace").unwrap().unwrap()))
        );
        // 修飾なしの文字キーと、名前の無いキー (0xBA) は確定せず、理由を返す。F キーと PrintScreen は確定する。
        assert_eq!(feed(&[d(0x53)]).0, Some(Recorded::NeedsModifier(0x53)));
        assert_eq!(feed(&[d(CTRL), d(0xBA)]).0, Some(Recorded::Unsupported));
        assert_eq!(
            feed(&[d(0x7C)]).0,
            Some(Recorded::Key(parse("F13").unwrap().unwrap()))
        );
        assert_eq!(
            feed(&[d(SHIFT), d(VK_SNAPSHOT)]).0,
            Some(Recorded::Key(parse("Shift+PrintScreen").unwrap().unwrap()))
        );
        // 左右の修飾キーの VK も数える。
        assert_eq!(
            feed(&[d(0xA2), d(0xA4), d(0x50)]).0,
            Some(Recorded::Key(parse("Ctrl+Alt+P").unwrap().unwrap()))
        );
        // 離しでは確定しない。
        assert_eq!(feed(&[d(CTRL), u(0x50)]).0, None);
    }

    #[test]
    fn keycaps() {
        assert_eq!(
            caps_of(parse("Ctrl+Alt+Shift+R").unwrap().unwrap()),
            ["Ctrl", "Alt", "Shift", "R"]
        );
    }

    #[test]
    fn roundtrip() {
        for (text, mods, vk) in [
            ("Shift+PrintScreen", MOD_SHIFT, VK_SNAPSHOT),
            ("Ctrl+Alt+Shift+S", MOD_CONTROL | MOD_ALT | MOD_SHIFT, 0x53),
            ("PrintScreen", 0, VK_SNAPSHOT),
            ("Win+F12", MOD_WIN, 0x7B),
            ("Ctrl+1", MOD_CONTROL, 0x31),
        ] {
            let parsed = parse(text).unwrap().unwrap();
            assert_eq!(parsed, Hotkey { mods, vk }, "{text}");
            assert_eq!(format(Some(parsed)), text);
        }
    }

    #[test]
    fn unassigned_roundtrip() {
        assert_eq!(parse("").unwrap(), None);
        assert_eq!(format(None), "");
    }

    #[test]
    fn parse_is_lenient_on_order_and_case() {
        assert_eq!(
            parse("shift + alt + ctrl + s").unwrap(),
            parse("Ctrl+Alt+Shift+S").unwrap()
        );
    }

    #[test]
    fn rejects_unknown() {
        assert!(parse("Hyper+S").is_err());
        assert!(parse("Ctrl+NoSuchKey").is_err());
        assert!(parse("Ctrl+").is_err());
    }
}
