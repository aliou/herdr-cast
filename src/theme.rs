//! Picker palette resolution.
//!
//! Reads the ten token values the pickers consume from herdr's config
//! (`HERDR_CONFIG_PATH`, else `XDG_CONFIG_HOME/herdr/config.toml`, else
//! `~/.config/herdr/config.toml` — herdr's own `config_path()` resolution), and picks
//! the mode by querying the terminal's actual background through OSC 11.
//! OSC 11 stays truthful inside herdr panes even when a forced Ghostty
//! theme makes the `?997` color-scheme report lie. Values missing from the
//! config fall back to the hard-coded senzu palette so an absent or
//! unparsable config cannot make a picker unreadable. Built-in base themes
//! other than `terminal` are not reproduced; their tokens resolve to the
//! senzu fallback unless the config carries a custom value.

use std::path::PathBuf;
use std::sync::OnceLock;

use ratatui::style::Color;

/// Picker palette roles mapped from herdr's theme tokens.
#[derive(Debug, PartialEq)]
pub struct Palette {
    pub background: Color, // panel_bg
    pub foreground: Color, // text
    pub accent: Color,     // accent
    pub selection: Color,  // selection_bg
    pub muted: Color,      // overlay0
    pub disabled: Color,   // overlay1
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub teal: Color,
}

/// Hard-coded senzu palette: the fallback for missing config values.
const FALLBACK: Palette = Palette {
    background: Color::Rgb(0x15, 0x15, 0x15),
    foreground: Color::Rgb(0xe8, 0xe8, 0xd3),
    accent: Color::Rgb(0x8f, 0xbf, 0xdc),
    selection: Color::Rgb(0x40, 0x40, 0x40),
    muted: Color::Rgb(0x60, 0x59, 0x58),
    disabled: Color::Rgb(0x88, 0x88, 0x88),
    red: Color::Rgb(0xd7, 0x45, 0x45),
    green: Color::Rgb(0x99, 0xad, 0x6a),
    yellow: Color::Rgb(0xfa, 0xd0, 0x7a),
    teal: Color::Rgb(0x66, 0x87, 0x99),
};

static PALETTE: OnceLock<Palette> = OnceLock::new();

/// Resolve the palette once per process.
pub fn palette() -> &'static Palette {
    PALETTE.get_or_init(|| {
        let light = is_light_terminal();
        resolve(config_file().as_deref(), light)
    })
}

/// Light appearance means luma >= 128 on the 0..255 scale, the same
/// threshold herdr applies to the OSC 11 background answer.
fn is_light((r, g, b): (u8, u8, u8)) -> bool {
    u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114 >= 128_000
}

fn is_light_terminal() -> bool {
    match osc11_background() {
        Some(background) => is_light(background),
        None => colorfgbg_is_light(),
    }
}

/// `COLORFGBG` is `<fg>;<bg>`: `15;0` is white-on-black, `0;15` light.
/// Unset or unparsable resolves to dark.
fn colorfgbg_is_light() -> bool {
    std::env::var("COLORFGBG")
        .ok()
        .and_then(|value| value.rsplit(';').next()?.parse::<u8>().ok())
        .is_some_and(|bg| bg >= 8)
}

/// Query the terminal background (OSC 11) on /dev/tty. The query runs with
/// its own termios save/restore, so it is safe before or around crossterm's
/// raw mode. Answers arrive as `ESC ] 11 ; rgb:RRRR/GGGG/BBBB BEL` or the
/// same terminated by `ESC \`.
fn osc11_background() -> Option<(u8, u8, u8)> {
    let flags = libc::O_RDWR | libc::O_NOCTTY;
    let fd = unsafe { libc::open(b"/dev/tty\0".as_ptr().cast(), flags) };
    if fd < 0 {
        return None;
    }
    unsafe {
        let mut saved: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut saved) != 0 {
            libc::close(fd);
            return None;
        }
        let mut raw = saved;
        libc::cfmakeraw(&mut raw);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 2;
        if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
            libc::close(fd);
            return None;
        }
        let query = b"\x1b]11;?\x1b\\";
        let mut reply = [0u8; 64];
        let mut len = 0;
        if libc::write(fd, query.as_ptr().cast(), query.len()) == query.len() as isize {
            for _ in 0..3 {
                let read = libc::read(fd, reply.as_mut_ptr().add(len).cast(), reply.len() - len);
                if read <= 0 {
                    break;
                }
                len += read as usize;
                if len == reply.len() {
                    break;
                }
            }
        }
        libc::tcsetattr(fd, libc::TCSANOW, &saved);
        libc::close(fd);
        parse_osc11_reply(&reply[..len])
    }
}

/// Parse `ESC ] 11 ; rgb:R/G/B` (1-4 hex digits per channel) from a reply
/// fragment, terminated by BEL or the start of `ESC \`.
fn parse_osc11_reply(bytes: &[u8]) -> Option<(u8, u8, u8)> {
    let start = bytes.windows(5).position(|window| window == b"\x1b]11;")?;
    let rest = bytes[start + 5..].strip_prefix(b"rgb:")?;
    let end = rest
        .iter()
        .position(|byte| *byte == 0x07 || *byte == 0x1b)?;
    let spec = std::str::from_utf8(&rest[..end]).ok()?;
    let mut channels = spec.split('/');
    let channel = |spec: &str| -> Option<u8> {
        if spec.is_empty() || spec.len() > 4 || !spec.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(spec, 16).ok()?;
        let max = (1 << (4 * spec.len())) - 1;
        Some(((value * 255 + max / 2) / max) as u8)
    };
    Some((
        channel(channels.next()?)?,
        channel(channels.next()?)?,
        channel(channels.next()?)?,
    ))
}

// --- config ---------------------------------------------------------------

#[derive(Default)]
struct Custom {
    background: Option<Color>,
    foreground: Option<Color>,
    accent: Option<Color>,
    selection: Option<Color>,
    muted: Option<Color>,
    disabled: Option<Color>,
    red: Option<Color>,
    green: Option<Color>,
    yellow: Option<Color>,
    teal: Option<Color>,
}

impl Custom {
    fn set(&mut self, key: &str, value: &str) {
        let Some(color) = parse_color(value) else {
            return;
        };
        match key {
            "panel_bg" => self.background = Some(color),
            "text" => self.foreground = Some(color),
            "accent" => self.accent = Some(color),
            "selection_bg" => self.selection = Some(color),
            "overlay0" => self.muted = Some(color),
            "overlay1" => self.disabled = Some(color),
            "red" => self.red = Some(color),
            "green" => self.green = Some(color),
            "yellow" => self.yellow = Some(color),
            "teal" => self.teal = Some(color),
            _ => {}
        }
    }
}

struct ThemeFile {
    auto_switch: bool,
    custom: Custom,
    dark: Custom,
    light: Custom,
}

impl Default for ThemeFile {
    fn default() -> Self {
        ThemeFile {
            auto_switch: false,
            custom: Custom::default(),
            dark: Custom::default(),
            light: Custom::default(),
        }
    }
}

/// Line-oriented parse of the theme section. Everything outside
/// `[theme]`/`[theme.custom...]` is ignored, which covers the arrays and
/// command strings elsewhere in herdr's config without a TOML dependency.
fn parse_theme(text: &str) -> ThemeFile {
    #[derive(Clone, Copy, PartialEq)]
    enum Section {
        Other,
        Theme,
        Custom,
        CustomDark,
        CustomLight,
    }
    let mut section = Section::Other;
    let mut file = ThemeFile::default();
    for line in text.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            section = match line.trim_start_matches('[').trim_end_matches(']') {
                "theme" => Section::Theme,
                "theme.custom" => Section::Custom,
                "theme.custom.dark" => Section::CustomDark,
                "theme.custom.light" => Section::CustomLight,
                _ => Section::Other,
            };
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = unquote(value.trim());
        match section {
            Section::Theme if key == "auto_switch" => file.auto_switch = value == "true",
            Section::Theme => {}
            Section::Custom => file.custom.set(key, &value),
            Section::CustomDark => file.dark.set(key, &value),
            Section::CustomLight => file.light.set(key, &value),
            Section::Other => {}
        }
    }
    file
}

/// Cut a `#` comment, ignoring `#` inside quoted strings.
fn strip_comment(line: &str) -> &str {
    let mut double = false;
    let mut single = false;
    for (index, ch) in line.char_indices() {
        match ch {
            '"' if !single => double = !double,
            '\'' if !double => single = !single,
            '#' if !double && !single => return &line[..index],
            _ => {}
        }
    }
    line
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    let quoted = bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''));
    if quoted {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

/// `#rgb` and `#rrggbb` hex colors.
fn parse_color(value: &str) -> Option<Color> {
    let hex = value.strip_prefix('#')?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match hex.len() {
        6 => {
            let channel = |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16).ok();
            Some(Color::Rgb(channel(0..2)?, channel(2..4)?, channel(4..6)?))
        }
        3 => {
            let digit = |index: usize| hex[index..index + 1].repeat(2);
            Some(Color::Rgb(
                u8::from_str_radix(&digit(0), 16).ok()?,
                u8::from_str_radix(&digit(1), 16).ok()?,
                u8::from_str_radix(&digit(2), 16).ok()?,
            ))
        }
        _ => None,
    }
}

fn resolve(config: Option<&str>, light: bool) -> Palette {
    let Some(file) = config.map(parse_theme) else {
        return FALLBACK;
    };
    let mode = file
        .auto_switch
        .then(|| if light { &file.light } else { &file.dark });
    let at = |mode_value: Option<Color>, flat_value: Option<Color>, fallback: Color| {
        mode_value.or(flat_value).unwrap_or(fallback)
    };
    Palette {
        background: at(
            mode.and_then(|m| m.background),
            file.custom.background,
            FALLBACK.background,
        ),
        foreground: at(
            mode.and_then(|m| m.foreground),
            file.custom.foreground,
            FALLBACK.foreground,
        ),
        accent: at(
            mode.and_then(|m| m.accent),
            file.custom.accent,
            FALLBACK.accent,
        ),
        selection: at(
            mode.and_then(|m| m.selection),
            file.custom.selection,
            FALLBACK.selection,
        ),
        muted: at(
            mode.and_then(|m| m.muted),
            file.custom.muted,
            FALLBACK.muted,
        ),
        disabled: at(
            mode.and_then(|m| m.disabled),
            file.custom.disabled,
            FALLBACK.disabled,
        ),
        red: at(mode.and_then(|m| m.red), file.custom.red, FALLBACK.red),
        green: at(
            mode.and_then(|m| m.green),
            file.custom.green,
            FALLBACK.green,
        ),
        yellow: at(
            mode.and_then(|m| m.yellow),
            file.custom.yellow,
            FALLBACK.yellow,
        ),
        teal: at(mode.and_then(|m| m.teal), file.custom.teal, FALLBACK.teal),
    }
}

/// herdr's own `config_path()` resolution, mirrored so the picker reads the
/// config herdr actually loaded. Release herdr uses the `herdr` app dir.
fn config_file() -> Option<String> {
    if let Some(path) = std::env::var_os("HERDR_CONFIG_PATH") {
        return std::fs::read_to_string(path).ok();
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    std::fs::read_to_string(base?.join("herdr").join("config.toml")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"
onboarding = false

[theme]
auto_switch = true
dark_name = "terminal"
light_name = "terminal"

# Full chrome palette mapped from senzu.
[theme.custom.dark]
accent = "#8fbfdc"        # panel.focused_border / morning_glory
panel_bg = "#151515"      # background
selection_bg = "#404040"
overlay0 = "#777777"
text = "#e8e8d3"
red = "#d74545"
green = "#99ad6a"
yellow = "#fad07a"
teal = "#668799"
overlay1 = "#888888"

[theme.custom.light]
accent = "#0a4a7a"
panel_bg = "#fafafa"
selection_bg = "#e8d0c0"
text = "#000000"
red = "#c00000"
teal = "#306070"

[[keys.command]]
command = 'herdr-cast open-popup --entrypoint hunk --pct-width 90'
description = "review changes with hunk"
key = "prefix+g"
type = "shell"

[ui.sidebar.spaces]
rows = [
  [ "state_icon", "workspace" ],
  [ { token = "$hostkind", fg = "#d98870" }, "$pad" ],
]
"##;

    #[test]
    fn parse_ignores_everything_but_theme_sections() {
        let file = parse_theme(SAMPLE);
        assert!(file.auto_switch);
        assert_eq!(file.dark.background, parse_color("#151515"));
        assert_eq!(file.dark.accent, parse_color("#8fbfdc"));
        assert_eq!(file.dark.muted, parse_color("#777777"));
        assert_eq!(file.light.selection, parse_color("#e8d0c0"));
        assert_eq!(file.light.foreground, parse_color("#000000"));
        // sidebar fg overrides and other config sections are not consumed
        assert_eq!(file.dark.teal, parse_color("#668799"));
    }

    #[test]
    fn auto_switch_picks_mode_block_over_flat_and_fallback() {
        let dark = resolve(Some(SAMPLE), false);
        assert_eq!(dark.background, Color::Rgb(0x15, 0x15, 0x15));
        assert_eq!(dark.foreground, Color::Rgb(0xe8, 0xe8, 0xd3));
        let light = resolve(Some(SAMPLE), true);
        assert_eq!(light.background, Color::Rgb(0xfa, 0xfa, 0xfa));
        assert_eq!(light.foreground, Color::Rgb(0x00, 0x00, 0x00));
        // tokens absent from the light block fall back to the senzu consts
        assert_eq!(light.muted, FALLBACK.muted);
        assert_eq!(light.green, FALLBACK.green);
    }

    #[test]
    fn flat_custom_applies_without_auto_switch() {
        let config = "[theme.custom]\npanel_bg = \"#010203\"\n";
        for light in [false, true] {
            assert_eq!(resolve(Some(config), light).background, Color::Rgb(1, 2, 3));
        }
    }

    #[test]
    fn missing_config_uses_fallback() {
        assert_eq!(resolve(None, false), FALLBACK);
        assert_eq!(resolve(Some("no theme here"), true), FALLBACK);
    }

    #[test]
    fn colors_parse_from_hex() {
        assert_eq!(parse_color("#8fbfdc"), Some(Color::Rgb(0x8f, 0xbf, 0xdc)));
        assert_eq!(parse_color("#8bd"), Some(Color::Rgb(0x88, 0xbb, 0xdd)));
        assert_eq!(parse_color("8fbfdc"), None);
        assert_eq!(parse_color("#8fbfd"), None);
        assert_eq!(parse_color("#zzzzzz"), None);
    }

    #[test]
    fn luma_threshold_matches_herdr() {
        assert!(!is_light((0x15, 0x15, 0x15)));
        assert!(!is_light((0x40, 0x40, 0x40)));
        assert!(is_light((0xf5, 0xe6, 0xd3)));
        assert!(is_light((0xfa, 0xfa, 0xfa)));
    }

    #[test]
    fn colorfgbg_classifies_background() {
        std::env::set_var("COLORFGBG", "15;0");
        assert!(!colorfgbg_is_light());
        std::env::set_var("COLORFGBG", "0;15");
        assert!(colorfgbg_is_light());
        std::env::set_var("COLORFGBG", "nonsense");
        assert!(!colorfgbg_is_light());
        std::env::remove_var("COLORFGBG");
        assert!(!colorfgbg_is_light());
    }

    #[test]
    fn osc_replies_parse_from_both_terminators() {
        // what herdr panes answer (ST-terminated)
        let st = b"\x1b[?997;2n\x1b]11;rgb:1515/1515/1515\x1b\\";
        assert_eq!(parse_osc11_reply(st), Some((0x15, 0x15, 0x15)));
        // what Ghostty answers directly (BEL-terminated)
        let bel = b"\x1b]11;rgb:f5f5/e6e6/d3d3\x07";
        assert_eq!(parse_osc11_reply(bel), Some((0xf5, 0xe6, 0xd3)));
        // short form scales to 8 bits
        assert_eq!(
            parse_osc11_reply(b"\x1b]11;rgb:80/ff/01\x07"),
            Some((0x80, 0xff, 0x01))
        );
        assert_eq!(parse_osc11_reply(b"\x1b]10;rgb:1/2/3\x07"), None);
        assert_eq!(parse_osc11_reply(b"garbage"), None);
    }
}
