//! A fixed-grid terminal cell canvas and its Rune renderer.
//!
//! Ported from `oracle/src/canvas.ts`, `format.ts` and `theme.ts` (the OpenTUI dashboard):
//! the view draws into a `Canvas` of cells exactly as the TypeScript view does, and
//! [`render_canvas`] paints it with Rune's native terminal [`rune::grid`]: one shaped line per
//! row in the embedded JetBrains Mono, with background runs as fills. The same fixed-pitch
//! font draws block, box and Braille graphics; inspected Linux output uses light hinting to
//! approach the reference terminal. Missing graphics use snapped vectors; T3_RUNE_GRAPHICS=vector
//! selects that diagnostic path explicitly. No per-cell elements or web styling.
//!
//! Font: JetBrainsMono Nerd Font 3.5.1 (Regular, Bold), SIL OFL 1.1, see
//! `assets/JetBrainsMonoNerdFont-OFL.txt`. Register [`FONTS`] with [`FAMILIES`] in the app spec;
//! text uses the family by name, never `.mono()`'s slot.
#![allow(dead_code)]

use std::collections::HashMap;

use crate::model::Agent;

// ── Font ────────────────────────────────────────────────────────────────────

/// The family name inside both files (fc-query: "JetBrainsMono Nerd Font").
pub const MONO_FAMILY: &str = "JetBrainsMono Nerd Font";
/// Advance width of every text glyph, in em (600 of 1000 units).
pub const ADVANCE_EM: f32 = 0.6;
/// Target cell metrics: 1280×800 points is 160×44 cells.
pub const CELL_W: f32 = 8.0;
pub const CELL_H: f32 = 18.0;

// ── Palette ─────────────────────────────────────────────────────────────────

pub type Rgb = u32; // 0xRRGGBB

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    pub bg: Rgb,
    /// Raised surface for tiles.
    pub panel: Rgb,
    /// Primary ink.
    pub fg: Rgb,
    /// Secondary ink; also readable on the selected row.
    pub text2: Rgb,
    /// Tertiary ink, captions.
    pub dim: Rgb,
    /// Non-text marks only: grid ticks, tree guides.
    pub faint: Rgb,
    pub track: Rgb,
    /// Lowest `ramp` position that reads at 3:1 against a context bar's track.
    pub floor: f64,
    pub rule: Rgb,
    pub accent: Rgb,
    pub bright: Rgb,
    pub hot: Rgb,
    pub warn: Rgb,
    pub sel: Rgb,
    pub busy: Rgb,
    /// Provider identity: claude, openai, grok, other.
    pub providers: [Rgb; 4],
}

impl Default for Palette {
    /// The baseline capture's palette: `derivePalette(THEMES.tokyo)` from
    /// `oracle/test/themes.ts` (TokyoNight: background #1a1b26, foreground #c0caf5,
    /// accent #7aa2f7, selection #33467c, red #f7768e, yellow #e0af68, green #9ece6a,
    /// magenta #bb9af7), values printed by the TypeScript `derivePalette` itself.
    fn default() -> Self {
        Palette::tokyo()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderFamily {
    Claude,
    OpenAi,
    Grok,
    Other,
}

impl Palette {
    pub const fn tokyo() -> Palette {
        Palette {
            dark: true,
            bg: 0x1a1b26,
            panel: 0x21222e,
            fg: 0xc0caf5,
            text2: 0xacb5dc,
            dim: 0x8188a6,
            faint: 0x45495c,
            track: 0x313443,
            floor: 0.31,
            rule: 0x3b3e4f,
            accent: 0x7aa2f7,
            bright: 0xe0e5fa,
            hot: 0xf7768e,
            warn: 0xe0af68,
            sel: 0x33467c,
            busy: 0x293147,
            providers: [0xbb9af7, 0x9ece6a, 0x7aa2f7, 0x6a6f89],
        }
    }
    /// `derivePalette({})`: the dashboard's fallback when no theme file is readable.
    pub const fn fallback() -> Palette {
        Palette {
            dark: true,
            bg: 0x101315,
            panel: 0x171a1c,
            fg: 0xcacccc,
            text2: 0x919494,
            dim: 0x7f8282,
            faint: 0x404345,
            track: 0x2a2d2f,
            floor: 0.04,
            rule: 0x35383a,
            accent: 0xcacccc,
            bright: 0xe5e6e6,
            hot: 0xde6145,
            warn: 0xd49789,
            sel: 0x2a2d2f,
            busy: 0x2e3132,
            providers: [0x8b5896, 0x1f9b82, 0x4c94ec, 0x696c6d],
        }
    }
    pub fn family_color(&self, f: ProviderFamily) -> Rgb {
        self.providers[f as usize]
    }
    /// Provider identity color of an agent's model (falling back to its provider id).
    pub fn provider(&self, a: &Agent) -> Rgb {
        self.family_color(family(&a.model, a.provider_id.as_deref()))
    }
    pub fn provider_of(&self, model: &str) -> Rgb {
        self.family_color(family(model, None))
    }
}

/// [`Palette::provider`] on the default palette.
pub fn provider(a: &Agent) -> Rgb {
    Palette::default().provider(a)
}

/// [`family`] for a run of calls that often repeat the previous arguments (agents of one group,
/// columns that sample one point): a repeat returns the previous answer. `family` is a pure
/// function of its two strings, so the answers are the same.
#[derive(Default)]
pub struct FamilyMemo<'a> {
    last: Option<(&'a str, Option<&'a str>, ProviderFamily)>,
}

impl<'a> FamilyMemo<'a> {
    pub fn family(&mut self, model: &'a str, hint: Option<&'a str>) -> ProviderFamily {
        if let Some((m, h, f)) = self.last {
            if m == model && h == hint {
                return f;
            }
        }
        let f = family(model, hint);
        self.last = Some((model, hint, f));
        f
    }
}

/// Model provider family from a model name, falling back to a provider id hint (theme.ts `family`).
pub fn family(model: &str, hint: Option<&str>) -> ProviderFamily {
    fn test(raw: &str) -> Option<ProviderFamily> {
        // `to_lowercase` of ASCII text is its ASCII lowercase: do that in place for the usual
        // short ASCII model name, without allocating. Anything else takes `to_lowercase`.
        let mut buf = [0u8; 64];
        let lowered;
        let s: &str = if raw.is_ascii() && raw.len() <= buf.len() {
            let b = &mut buf[..raw.len()];
            b.copy_from_slice(raw.as_bytes());
            b.make_ascii_lowercase();
            std::str::from_utf8(b).expect("ASCII")
        } else {
            lowered = raw.to_lowercase();
            &lowered
        };
        if ["claude", "opus", "sonnet", "haiku", "fable", "anthropic"]
            .iter()
            .any(|k| s.contains(k))
        {
            return Some(ProviderFamily::Claude);
        }
        // /gpt|codex|openai|\bo[1-9]\b/
        let o_model = s.as_bytes().windows(2).enumerate().any(|(i, w)| {
            w[0] == b'o'
                && (b'1'..=b'9').contains(&w[1])
                && (i == 0 || !is_word(s.as_bytes()[i - 1]))
                && s.as_bytes().get(i + 2).is_none_or(|&c| !is_word(c))
        });
        if s.contains("gpt") || s.contains("codex") || s.contains("openai") || o_model {
            return Some(ProviderFamily::OpenAi);
        }
        if s.contains("grok") || s.contains("xai") {
            return Some(ProviderFamily::Grok);
        }
        None
    }
    fn is_word(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'_'
    }
    test(model)
        .or_else(|| hint.and_then(test))
        .unwrap_or(ProviderFamily::Other)
}

/// `a` moved `t` of the way to `b` in sRGB, rounded as the TypeScript `mix` rounds.
pub fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let k = t.clamp(0.0, 1.0);
    let ch = |shift: u32| {
        let (x, y) = (((a >> shift) & 255) as f64, ((b >> shift) & 255) as f64);
        ((x + (y - x) * k + 0.5).floor() as u32).min(255) << shift
    };
    ch(16) | ch(8) | ch(0)
}

/// Sequential ramp: magnitude reads as brightness (half-accent → accent → bright).
pub fn ramp(p: &Palette, t: f64) -> Rgb {
    let k = t.clamp(0.0, 1.0);
    if k < 0.55 {
        mix(mix(p.accent, p.bg, 0.5), p.accent, k / 0.55)
    } else {
        mix(p.accent, p.bright, (k - 0.55) / 0.45)
    }
}

/// A position along a bar for [`ramp`] or [`mix`]: `base + span * (i / cells)`, grouped the way
/// the original writes it. `base + span * i / cells` is not the same sum: the two differ in
/// the last bit for some `i` and `cells`, and that bit decides a rounding tie in `mix`. Written
/// the second way, one cell of a meter came out one color step off at 46 bar positions among
/// widths 1 to 300. Keep the division first.
pub fn along(base: f64, span: f64, i: f64, cells: f64) -> f64 {
    base + span * (i / cells)
}

// ── Text measurement (format.ts) ────────────────────────────────────────────

const WIDE: [(u32, u32); 15] = [
    (0x1100, 0x115f),
    (0x2e80, 0x303e),
    (0x3041, 0x33ff),
    (0x3400, 0x4dbf),
    (0x4e00, 0x9fff),
    (0xa000, 0xa4cf),
    (0xac00, 0xd7a3),
    (0xf900, 0xfaff),
    (0xfe30, 0xfe4f),
    (0xff00, 0xff60),
    (0xffe0, 0xffe6),
    (0x1f300, 0x1f64f),
    (0x1f900, 0x1f9ff),
    (0x1fa70, 0x1faff),
    (0x20000, 0x3fffd),
];

/// Columns a code point takes: 0 for combining marks, zero-width and variation selectors, 2 for
/// East Asian wide and emoji, else 1 (format.ts `cellWidth`).
pub fn cell_width(c: char) -> usize {
    let cp = c as u32;
    if cp < 0x300 {
        return 1;
    }
    if (0x300..=0x36f).contains(&cp)
        || (0x200b..=0x200f).contains(&cp)
        || (0xfe00..=0xfe0f).contains(&cp)
    {
        return 0;
    }
    if WIDE.iter().any(|&(a, b)| (a..=b).contains(&cp)) {
        2
    } else {
        1
    }
}

/// Display width in columns (format.ts `textWidth`).
pub fn width(s: &str) -> usize {
    s.chars().map(cell_width).sum()
}

// BEGIN GENERATED HIDDEN RANGES (Unicode 17.0)
pub(crate) static HIDDEN_RANGES: &[(u32, u32)] = &[
    (0x0, 0x1f),
    (0x7f, 0x9f),
    (0xad, 0xad),
    (0x378, 0x379),
    (0x380, 0x383),
    (0x38b, 0x38b),
    (0x38d, 0x38d),
    (0x3a2, 0x3a2),
    (0x530, 0x530),
    (0x557, 0x558),
    (0x58b, 0x58c),
    (0x590, 0x590),
    (0x5c8, 0x5cf),
    (0x5eb, 0x5ee),
    (0x5f5, 0x605),
    (0x61c, 0x61c),
    (0x6dd, 0x6dd),
    (0x70e, 0x70f),
    (0x74b, 0x74c),
    (0x7b2, 0x7bf),
    (0x7fb, 0x7fc),
    (0x82e, 0x82f),
    (0x83f, 0x83f),
    (0x85c, 0x85d),
    (0x85f, 0x85f),
    (0x86b, 0x86f),
    (0x890, 0x896),
    (0x8e2, 0x8e2),
    (0x984, 0x984),
    (0x98d, 0x98e),
    (0x991, 0x992),
    (0x9a9, 0x9a9),
    (0x9b1, 0x9b1),
    (0x9b3, 0x9b5),
    (0x9ba, 0x9bb),
    (0x9c5, 0x9c6),
    (0x9c9, 0x9ca),
    (0x9cf, 0x9d6),
    (0x9d8, 0x9db),
    (0x9de, 0x9de),
    (0x9e4, 0x9e5),
    (0x9ff, 0xa00),
    (0xa04, 0xa04),
    (0xa0b, 0xa0e),
    (0xa11, 0xa12),
    (0xa29, 0xa29),
    (0xa31, 0xa31),
    (0xa34, 0xa34),
    (0xa37, 0xa37),
    (0xa3a, 0xa3b),
    (0xa3d, 0xa3d),
    (0xa43, 0xa46),
    (0xa49, 0xa4a),
    (0xa4e, 0xa50),
    (0xa52, 0xa58),
    (0xa5d, 0xa5d),
    (0xa5f, 0xa65),
    (0xa77, 0xa80),
    (0xa84, 0xa84),
    (0xa8e, 0xa8e),
    (0xa92, 0xa92),
    (0xaa9, 0xaa9),
    (0xab1, 0xab1),
    (0xab4, 0xab4),
    (0xaba, 0xabb),
    (0xac6, 0xac6),
    (0xaca, 0xaca),
    (0xace, 0xacf),
    (0xad1, 0xadf),
    (0xae4, 0xae5),
    (0xaf2, 0xaf8),
    (0xb00, 0xb00),
    (0xb04, 0xb04),
    (0xb0d, 0xb0e),
    (0xb11, 0xb12),
    (0xb29, 0xb29),
    (0xb31, 0xb31),
    (0xb34, 0xb34),
    (0xb3a, 0xb3b),
    (0xb45, 0xb46),
    (0xb49, 0xb4a),
    (0xb4e, 0xb54),
    (0xb58, 0xb5b),
    (0xb5e, 0xb5e),
    (0xb64, 0xb65),
    (0xb78, 0xb81),
    (0xb84, 0xb84),
    (0xb8b, 0xb8d),
    (0xb91, 0xb91),
    (0xb96, 0xb98),
    (0xb9b, 0xb9b),
    (0xb9d, 0xb9d),
    (0xba0, 0xba2),
    (0xba5, 0xba7),
    (0xbab, 0xbad),
    (0xbba, 0xbbd),
    (0xbc3, 0xbc5),
    (0xbc9, 0xbc9),
    (0xbce, 0xbcf),
    (0xbd1, 0xbd6),
    (0xbd8, 0xbe5),
    (0xbfb, 0xbff),
    (0xc0d, 0xc0d),
    (0xc11, 0xc11),
    (0xc29, 0xc29),
    (0xc3a, 0xc3b),
    (0xc45, 0xc45),
    (0xc49, 0xc49),
    (0xc4e, 0xc54),
    (0xc57, 0xc57),
    (0xc5b, 0xc5b),
    (0xc5e, 0xc5f),
    (0xc64, 0xc65),
    (0xc70, 0xc76),
    (0xc8d, 0xc8d),
    (0xc91, 0xc91),
    (0xca9, 0xca9),
    (0xcb4, 0xcb4),
    (0xcba, 0xcbb),
    (0xcc5, 0xcc5),
    (0xcc9, 0xcc9),
    (0xcce, 0xcd4),
    (0xcd7, 0xcdb),
    (0xcdf, 0xcdf),
    (0xce4, 0xce5),
    (0xcf0, 0xcf0),
    (0xcf4, 0xcff),
    (0xd0d, 0xd0d),
    (0xd11, 0xd11),
    (0xd45, 0xd45),
    (0xd49, 0xd49),
    (0xd50, 0xd53),
    (0xd64, 0xd65),
    (0xd80, 0xd80),
    (0xd84, 0xd84),
    (0xd97, 0xd99),
    (0xdb2, 0xdb2),
    (0xdbc, 0xdbc),
    (0xdbe, 0xdbf),
    (0xdc7, 0xdc9),
    (0xdcb, 0xdce),
    (0xdd5, 0xdd5),
    (0xdd7, 0xdd7),
    (0xde0, 0xde5),
    (0xdf0, 0xdf1),
    (0xdf5, 0xe00),
    (0xe3b, 0xe3e),
    (0xe5c, 0xe80),
    (0xe83, 0xe83),
    (0xe85, 0xe85),
    (0xe8b, 0xe8b),
    (0xea4, 0xea4),
    (0xea6, 0xea6),
    (0xebe, 0xebf),
    (0xec5, 0xec5),
    (0xec7, 0xec7),
    (0xecf, 0xecf),
    (0xeda, 0xedb),
    (0xee0, 0xeff),
    (0xf48, 0xf48),
    (0xf6d, 0xf70),
    (0xf98, 0xf98),
    (0xfbd, 0xfbd),
    (0xfcd, 0xfcd),
    (0xfdb, 0xfff),
    (0x10c6, 0x10c6),
    (0x10c8, 0x10cc),
    (0x10ce, 0x10cf),
    (0x1249, 0x1249),
    (0x124e, 0x124f),
    (0x1257, 0x1257),
    (0x1259, 0x1259),
    (0x125e, 0x125f),
    (0x1289, 0x1289),
    (0x128e, 0x128f),
    (0x12b1, 0x12b1),
    (0x12b6, 0x12b7),
    (0x12bf, 0x12bf),
    (0x12c1, 0x12c1),
    (0x12c6, 0x12c7),
    (0x12d7, 0x12d7),
    (0x1311, 0x1311),
    (0x1316, 0x1317),
    (0x135b, 0x135c),
    (0x137d, 0x137f),
    (0x139a, 0x139f),
    (0x13f6, 0x13f7),
    (0x13fe, 0x13ff),
    (0x169d, 0x169f),
    (0x16f9, 0x16ff),
    (0x1716, 0x171e),
    (0x1737, 0x173f),
    (0x1754, 0x175f),
    (0x176d, 0x176d),
    (0x1771, 0x1771),
    (0x1774, 0x177f),
    (0x17de, 0x17df),
    (0x17ea, 0x17ef),
    (0x17fa, 0x17ff),
    (0x180e, 0x180e),
    (0x181a, 0x181f),
    (0x1879, 0x187f),
    (0x18ab, 0x18af),
    (0x18f6, 0x18ff),
    (0x191f, 0x191f),
    (0x192c, 0x192f),
    (0x193c, 0x193f),
    (0x1941, 0x1943),
    (0x196e, 0x196f),
    (0x1975, 0x197f),
    (0x19ac, 0x19af),
    (0x19ca, 0x19cf),
    (0x19db, 0x19dd),
    (0x1a1c, 0x1a1d),
    (0x1a5f, 0x1a5f),
    (0x1a7d, 0x1a7e),
    (0x1a8a, 0x1a8f),
    (0x1a9a, 0x1a9f),
    (0x1aae, 0x1aaf),
    (0x1ade, 0x1adf),
    (0x1aec, 0x1aff),
    (0x1b4d, 0x1b4d),
    (0x1bf4, 0x1bfb),
    (0x1c38, 0x1c3a),
    (0x1c4a, 0x1c4c),
    (0x1c8b, 0x1c8f),
    (0x1cbb, 0x1cbc),
    (0x1cc8, 0x1ccf),
    (0x1cfb, 0x1cff),
    (0x1f16, 0x1f17),
    (0x1f1e, 0x1f1f),
    (0x1f46, 0x1f47),
    (0x1f4e, 0x1f4f),
    (0x1f58, 0x1f58),
    (0x1f5a, 0x1f5a),
    (0x1f5c, 0x1f5c),
    (0x1f5e, 0x1f5e),
    (0x1f7e, 0x1f7f),
    (0x1fb5, 0x1fb5),
    (0x1fc5, 0x1fc5),
    (0x1fd4, 0x1fd5),
    (0x1fdc, 0x1fdc),
    (0x1ff0, 0x1ff1),
    (0x1ff5, 0x1ff5),
    (0x1fff, 0x1fff),
    (0x200b, 0x200f),
    (0x202a, 0x202e),
    (0x2060, 0x206f),
    (0x2072, 0x2073),
    (0x208f, 0x208f),
    (0x209d, 0x209f),
    (0x20c2, 0x20cf),
    (0x20f1, 0x20ff),
    (0x218c, 0x218f),
    (0x242a, 0x243f),
    (0x244b, 0x245f),
    (0x2b74, 0x2b75),
    (0x2cf4, 0x2cf8),
    (0x2d26, 0x2d26),
    (0x2d28, 0x2d2c),
    (0x2d2e, 0x2d2f),
    (0x2d68, 0x2d6e),
    (0x2d71, 0x2d7e),
    (0x2d97, 0x2d9f),
    (0x2da7, 0x2da7),
    (0x2daf, 0x2daf),
    (0x2db7, 0x2db7),
    (0x2dbf, 0x2dbf),
    (0x2dc7, 0x2dc7),
    (0x2dcf, 0x2dcf),
    (0x2dd7, 0x2dd7),
    (0x2ddf, 0x2ddf),
    (0x2e5e, 0x2e7f),
    (0x2e9a, 0x2e9a),
    (0x2ef4, 0x2eff),
    (0x2fd6, 0x2fef),
    (0x3040, 0x3040),
    (0x3097, 0x3098),
    (0x3100, 0x3104),
    (0x3130, 0x3130),
    (0x318f, 0x318f),
    (0x31e6, 0x31ee),
    (0x321f, 0x321f),
    (0xa48d, 0xa48f),
    (0xa4c7, 0xa4cf),
    (0xa62c, 0xa63f),
    (0xa6f8, 0xa6ff),
    (0xa7dd, 0xa7f0),
    (0xa82d, 0xa82f),
    (0xa83a, 0xa83f),
    (0xa878, 0xa87f),
    (0xa8c6, 0xa8cd),
    (0xa8da, 0xa8df),
    (0xa954, 0xa95e),
    (0xa97d, 0xa97f),
    (0xa9ce, 0xa9ce),
    (0xa9da, 0xa9dd),
    (0xa9ff, 0xa9ff),
    (0xaa37, 0xaa3f),
    (0xaa4e, 0xaa4f),
    (0xaa5a, 0xaa5b),
    (0xaac3, 0xaada),
    (0xaaf7, 0xab00),
    (0xab07, 0xab08),
    (0xab0f, 0xab10),
    (0xab17, 0xab1f),
    (0xab27, 0xab27),
    (0xab2f, 0xab2f),
    (0xab6c, 0xab6f),
    (0xabee, 0xabef),
    (0xabfa, 0xabff),
    (0xd7a4, 0xd7af),
    (0xd7c7, 0xd7ca),
    (0xd7fc, 0xf8ff),
    (0xfa6e, 0xfa6f),
    (0xfada, 0xfaff),
    (0xfb07, 0xfb12),
    (0xfb18, 0xfb1c),
    (0xfb37, 0xfb37),
    (0xfb3d, 0xfb3d),
    (0xfb3f, 0xfb3f),
    (0xfb42, 0xfb42),
    (0xfb45, 0xfb45),
    (0xfdd0, 0xfdef),
    (0xfe1a, 0xfe1f),
    (0xfe53, 0xfe53),
    (0xfe67, 0xfe67),
    (0xfe6c, 0xfe6f),
    (0xfe75, 0xfe75),
    (0xfefd, 0xff00),
    (0xffbf, 0xffc1),
    (0xffc8, 0xffc9),
    (0xffd0, 0xffd1),
    (0xffd8, 0xffd9),
    (0xffdd, 0xffdf),
    (0xffe7, 0xffe7),
    (0xffef, 0xfffb),
    (0xfffe, 0xffff),
    (0x1000c, 0x1000c),
    (0x10027, 0x10027),
    (0x1003b, 0x1003b),
    (0x1003e, 0x1003e),
    (0x1004e, 0x1004f),
    (0x1005e, 0x1007f),
    (0x100fb, 0x100ff),
    (0x10103, 0x10106),
    (0x10134, 0x10136),
    (0x1018f, 0x1018f),
    (0x1019d, 0x1019f),
    (0x101a1, 0x101cf),
    (0x101fe, 0x1027f),
    (0x1029d, 0x1029f),
    (0x102d1, 0x102df),
    (0x102fc, 0x102ff),
    (0x10324, 0x1032c),
    (0x1034b, 0x1034f),
    (0x1037b, 0x1037f),
    (0x1039e, 0x1039e),
    (0x103c4, 0x103c7),
    (0x103d6, 0x103ff),
    (0x1049e, 0x1049f),
    (0x104aa, 0x104af),
    (0x104d4, 0x104d7),
    (0x104fc, 0x104ff),
    (0x10528, 0x1052f),
    (0x10564, 0x1056e),
    (0x1057b, 0x1057b),
    (0x1058b, 0x1058b),
    (0x10593, 0x10593),
    (0x10596, 0x10596),
    (0x105a2, 0x105a2),
    (0x105b2, 0x105b2),
    (0x105ba, 0x105ba),
    (0x105bd, 0x105bf),
    (0x105f4, 0x105ff),
    (0x10737, 0x1073f),
    (0x10756, 0x1075f),
    (0x10768, 0x1077f),
    (0x10786, 0x10786),
    (0x107b1, 0x107b1),
    (0x107bb, 0x107ff),
    (0x10806, 0x10807),
    (0x10809, 0x10809),
    (0x10836, 0x10836),
    (0x10839, 0x1083b),
    (0x1083d, 0x1083e),
    (0x10856, 0x10856),
    (0x1089f, 0x108a6),
    (0x108b0, 0x108df),
    (0x108f3, 0x108f3),
    (0x108f6, 0x108fa),
    (0x1091c, 0x1091e),
    (0x1093a, 0x1093e),
    (0x1095a, 0x1097f),
    (0x109b8, 0x109bb),
    (0x109d0, 0x109d1),
    (0x10a04, 0x10a04),
    (0x10a07, 0x10a0b),
    (0x10a14, 0x10a14),
    (0x10a18, 0x10a18),
    (0x10a36, 0x10a37),
    (0x10a3b, 0x10a3e),
    (0x10a49, 0x10a4f),
    (0x10a59, 0x10a5f),
    (0x10aa0, 0x10abf),
    (0x10ae7, 0x10aea),
    (0x10af7, 0x10aff),
    (0x10b36, 0x10b38),
    (0x10b56, 0x10b57),
    (0x10b73, 0x10b77),
    (0x10b92, 0x10b98),
    (0x10b9d, 0x10ba8),
    (0x10bb0, 0x10bff),
    (0x10c49, 0x10c7f),
    (0x10cb3, 0x10cbf),
    (0x10cf3, 0x10cf9),
    (0x10d28, 0x10d2f),
    (0x10d3a, 0x10d3f),
    (0x10d66, 0x10d68),
    (0x10d86, 0x10d8d),
    (0x10d90, 0x10e5f),
    (0x10e7f, 0x10e7f),
    (0x10eaa, 0x10eaa),
    (0x10eae, 0x10eaf),
    (0x10eb2, 0x10ec1),
    (0x10ec8, 0x10ecf),
    (0x10ed9, 0x10ef9),
    (0x10f28, 0x10f2f),
    (0x10f5a, 0x10f6f),
    (0x10f8a, 0x10faf),
    (0x10fcc, 0x10fdf),
    (0x10ff7, 0x10fff),
    (0x1104e, 0x11051),
    (0x11076, 0x1107e),
    (0x110bd, 0x110bd),
    (0x110c3, 0x110cf),
    (0x110e9, 0x110ef),
    (0x110fa, 0x110ff),
    (0x11135, 0x11135),
    (0x11148, 0x1114f),
    (0x11177, 0x1117f),
    (0x111e0, 0x111e0),
    (0x111f5, 0x111ff),
    (0x11212, 0x11212),
    (0x11242, 0x1127f),
    (0x11287, 0x11287),
    (0x11289, 0x11289),
    (0x1128e, 0x1128e),
    (0x1129e, 0x1129e),
    (0x112aa, 0x112af),
    (0x112eb, 0x112ef),
    (0x112fa, 0x112ff),
    (0x11304, 0x11304),
    (0x1130d, 0x1130e),
    (0x11311, 0x11312),
    (0x11329, 0x11329),
    (0x11331, 0x11331),
    (0x11334, 0x11334),
    (0x1133a, 0x1133a),
    (0x11345, 0x11346),
    (0x11349, 0x1134a),
    (0x1134e, 0x1134f),
    (0x11351, 0x11356),
    (0x11358, 0x1135c),
    (0x11364, 0x11365),
    (0x1136d, 0x1136f),
    (0x11375, 0x1137f),
    (0x1138a, 0x1138a),
    (0x1138c, 0x1138d),
    (0x1138f, 0x1138f),
    (0x113b6, 0x113b6),
    (0x113c1, 0x113c1),
    (0x113c3, 0x113c4),
    (0x113c6, 0x113c6),
    (0x113cb, 0x113cb),
    (0x113d6, 0x113d6),
    (0x113d9, 0x113e0),
    (0x113e3, 0x113ff),
    (0x1145c, 0x1145c),
    (0x11462, 0x1147f),
    (0x114c8, 0x114cf),
    (0x114da, 0x1157f),
    (0x115b6, 0x115b7),
    (0x115de, 0x115ff),
    (0x11645, 0x1164f),
    (0x1165a, 0x1165f),
    (0x1166d, 0x1167f),
    (0x116ba, 0x116bf),
    (0x116ca, 0x116cf),
    (0x116e4, 0x116ff),
    (0x1171b, 0x1171c),
    (0x1172c, 0x1172f),
    (0x11747, 0x117ff),
    (0x1183c, 0x1189f),
    (0x118f3, 0x118fe),
    (0x11907, 0x11908),
    (0x1190a, 0x1190b),
    (0x11914, 0x11914),
    (0x11917, 0x11917),
    (0x11936, 0x11936),
    (0x11939, 0x1193a),
    (0x11947, 0x1194f),
    (0x1195a, 0x1199f),
    (0x119a8, 0x119a9),
    (0x119d8, 0x119d9),
    (0x119e5, 0x119ff),
    (0x11a48, 0x11a4f),
    (0x11aa3, 0x11aaf),
    (0x11af9, 0x11aff),
    (0x11b0a, 0x11b5f),
    (0x11b68, 0x11bbf),
    (0x11be2, 0x11bef),
    (0x11bfa, 0x11bff),
    (0x11c09, 0x11c09),
    (0x11c37, 0x11c37),
    (0x11c46, 0x11c4f),
    (0x11c6d, 0x11c6f),
    (0x11c90, 0x11c91),
    (0x11ca8, 0x11ca8),
    (0x11cb7, 0x11cff),
    (0x11d07, 0x11d07),
    (0x11d0a, 0x11d0a),
    (0x11d37, 0x11d39),
    (0x11d3b, 0x11d3b),
    (0x11d3e, 0x11d3e),
    (0x11d48, 0x11d4f),
    (0x11d5a, 0x11d5f),
    (0x11d66, 0x11d66),
    (0x11d69, 0x11d69),
    (0x11d8f, 0x11d8f),
    (0x11d92, 0x11d92),
    (0x11d99, 0x11d9f),
    (0x11daa, 0x11daf),
    (0x11ddc, 0x11ddf),
    (0x11dea, 0x11edf),
    (0x11ef9, 0x11eff),
    (0x11f11, 0x11f11),
    (0x11f3b, 0x11f3d),
    (0x11f5b, 0x11faf),
    (0x11fb1, 0x11fbf),
    (0x11ff2, 0x11ffe),
    (0x1239a, 0x123ff),
    (0x1246f, 0x1246f),
    (0x12475, 0x1247f),
    (0x12544, 0x12f8f),
    (0x12ff3, 0x12fff),
    (0x13430, 0x1343f),
    (0x13456, 0x1345f),
    (0x143fb, 0x143ff),
    (0x14647, 0x160ff),
    (0x1613a, 0x167ff),
    (0x16a39, 0x16a3f),
    (0x16a5f, 0x16a5f),
    (0x16a6a, 0x16a6d),
    (0x16abf, 0x16abf),
    (0x16aca, 0x16acf),
    (0x16aee, 0x16aef),
    (0x16af6, 0x16aff),
    (0x16b46, 0x16b4f),
    (0x16b5a, 0x16b5a),
    (0x16b62, 0x16b62),
    (0x16b78, 0x16b7c),
    (0x16b90, 0x16d3f),
    (0x16d7a, 0x16e3f),
    (0x16e9b, 0x16e9f),
    (0x16eb9, 0x16eba),
    (0x16ed4, 0x16eff),
    (0x16f4b, 0x16f4e),
    (0x16f88, 0x16f8e),
    (0x16fa0, 0x16fdf),
    (0x16fe5, 0x16fef),
    (0x16ff7, 0x16fff),
    (0x18cd6, 0x18cfe),
    (0x18d1f, 0x18d7f),
    (0x18df3, 0x1afef),
    (0x1aff4, 0x1aff4),
    (0x1affc, 0x1affc),
    (0x1afff, 0x1afff),
    (0x1b123, 0x1b131),
    (0x1b133, 0x1b14f),
    (0x1b153, 0x1b154),
    (0x1b156, 0x1b163),
    (0x1b168, 0x1b16f),
    (0x1b2fc, 0x1bbff),
    (0x1bc6b, 0x1bc6f),
    (0x1bc7d, 0x1bc7f),
    (0x1bc89, 0x1bc8f),
    (0x1bc9a, 0x1bc9b),
    (0x1bca0, 0x1cbff),
    (0x1ccfd, 0x1ccff),
    (0x1ceb4, 0x1ceb9),
    (0x1ced1, 0x1cedf),
    (0x1cef1, 0x1ceff),
    (0x1cf2e, 0x1cf2f),
    (0x1cf47, 0x1cf4f),
    (0x1cfc4, 0x1cfff),
    (0x1d0f6, 0x1d0ff),
    (0x1d127, 0x1d128),
    (0x1d173, 0x1d17a),
    (0x1d1eb, 0x1d1ff),
    (0x1d246, 0x1d2bf),
    (0x1d2d4, 0x1d2df),
    (0x1d2f4, 0x1d2ff),
    (0x1d357, 0x1d35f),
    (0x1d379, 0x1d3ff),
    (0x1d455, 0x1d455),
    (0x1d49d, 0x1d49d),
    (0x1d4a0, 0x1d4a1),
    (0x1d4a3, 0x1d4a4),
    (0x1d4a7, 0x1d4a8),
    (0x1d4ad, 0x1d4ad),
    (0x1d4ba, 0x1d4ba),
    (0x1d4bc, 0x1d4bc),
    (0x1d4c4, 0x1d4c4),
    (0x1d506, 0x1d506),
    (0x1d50b, 0x1d50c),
    (0x1d515, 0x1d515),
    (0x1d51d, 0x1d51d),
    (0x1d53a, 0x1d53a),
    (0x1d53f, 0x1d53f),
    (0x1d545, 0x1d545),
    (0x1d547, 0x1d549),
    (0x1d551, 0x1d551),
    (0x1d6a6, 0x1d6a7),
    (0x1d7cc, 0x1d7cd),
    (0x1da8c, 0x1da9a),
    (0x1daa0, 0x1daa0),
    (0x1dab0, 0x1deff),
    (0x1df1f, 0x1df24),
    (0x1df2b, 0x1dfff),
    (0x1e007, 0x1e007),
    (0x1e019, 0x1e01a),
    (0x1e022, 0x1e022),
    (0x1e025, 0x1e025),
    (0x1e02b, 0x1e02f),
    (0x1e06e, 0x1e08e),
    (0x1e090, 0x1e0ff),
    (0x1e12d, 0x1e12f),
    (0x1e13e, 0x1e13f),
    (0x1e14a, 0x1e14d),
    (0x1e150, 0x1e28f),
    (0x1e2af, 0x1e2bf),
    (0x1e2fa, 0x1e2fe),
    (0x1e300, 0x1e4cf),
    (0x1e4fa, 0x1e5cf),
    (0x1e5fb, 0x1e5fe),
    (0x1e600, 0x1e6bf),
    (0x1e6df, 0x1e6df),
    (0x1e6f6, 0x1e6fd),
    (0x1e700, 0x1e7df),
    (0x1e7e7, 0x1e7e7),
    (0x1e7ec, 0x1e7ec),
    (0x1e7ef, 0x1e7ef),
    (0x1e7ff, 0x1e7ff),
    (0x1e8c5, 0x1e8c6),
    (0x1e8d7, 0x1e8ff),
    (0x1e94c, 0x1e94f),
    (0x1e95a, 0x1e95d),
    (0x1e960, 0x1ec70),
    (0x1ecb5, 0x1ed00),
    (0x1ed3e, 0x1edff),
    (0x1ee04, 0x1ee04),
    (0x1ee20, 0x1ee20),
    (0x1ee23, 0x1ee23),
    (0x1ee25, 0x1ee26),
    (0x1ee28, 0x1ee28),
    (0x1ee33, 0x1ee33),
    (0x1ee38, 0x1ee38),
    (0x1ee3a, 0x1ee3a),
    (0x1ee3c, 0x1ee41),
    (0x1ee43, 0x1ee46),
    (0x1ee48, 0x1ee48),
    (0x1ee4a, 0x1ee4a),
    (0x1ee4c, 0x1ee4c),
    (0x1ee50, 0x1ee50),
    (0x1ee53, 0x1ee53),
    (0x1ee55, 0x1ee56),
    (0x1ee58, 0x1ee58),
    (0x1ee5a, 0x1ee5a),
    (0x1ee5c, 0x1ee5c),
    (0x1ee5e, 0x1ee5e),
    (0x1ee60, 0x1ee60),
    (0x1ee63, 0x1ee63),
    (0x1ee65, 0x1ee66),
    (0x1ee6b, 0x1ee6b),
    (0x1ee73, 0x1ee73),
    (0x1ee78, 0x1ee78),
    (0x1ee7d, 0x1ee7d),
    (0x1ee7f, 0x1ee7f),
    (0x1ee8a, 0x1ee8a),
    (0x1ee9c, 0x1eea0),
    (0x1eea4, 0x1eea4),
    (0x1eeaa, 0x1eeaa),
    (0x1eebc, 0x1eeef),
    (0x1eef2, 0x1efff),
    (0x1f02c, 0x1f02f),
    (0x1f094, 0x1f09f),
    (0x1f0af, 0x1f0b0),
    (0x1f0c0, 0x1f0c0),
    (0x1f0d0, 0x1f0d0),
    (0x1f0f6, 0x1f0ff),
    (0x1f1ae, 0x1f1e5),
    (0x1f203, 0x1f20f),
    (0x1f23c, 0x1f23f),
    (0x1f249, 0x1f24f),
    (0x1f252, 0x1f25f),
    (0x1f266, 0x1f2ff),
    (0x1f6d9, 0x1f6db),
    (0x1f6ed, 0x1f6ef),
    (0x1f6fd, 0x1f6ff),
    (0x1f7da, 0x1f7df),
    (0x1f7ec, 0x1f7ef),
    (0x1f7f1, 0x1f7ff),
    (0x1f80c, 0x1f80f),
    (0x1f848, 0x1f84f),
    (0x1f85a, 0x1f85f),
    (0x1f888, 0x1f88f),
    (0x1f8ae, 0x1f8af),
    (0x1f8bc, 0x1f8bf),
    (0x1f8c2, 0x1f8cf),
    (0x1f8d9, 0x1f8ff),
    (0x1fa58, 0x1fa5f),
    (0x1fa6e, 0x1fa6f),
    (0x1fa7d, 0x1fa7f),
    (0x1fa8b, 0x1fa8d),
    (0x1fac7, 0x1fac7),
    (0x1fac9, 0x1facc),
    (0x1fadd, 0x1fade),
    (0x1faeb, 0x1faee),
    (0x1faf9, 0x1faff),
    (0x1fb93, 0x1fb93),
    (0x1fbfb, 0x1ffff),
    (0x2a6e0, 0x2a6ff),
    (0x2b81e, 0x2b81f),
    (0x2ceae, 0x2ceaf),
    (0x2ebe1, 0x2ebef),
    (0x2ee5e, 0x2f7ff),
    (0x2fa1e, 0x2ffff),
    (0x3134b, 0x3134f),
    (0x3347a, 0xe00ff),
    (0xe01f0, 0x10ffff),
];
// END GENERATED HIDDEN RANGES

/// Unicode general category C (control, format, private use, surrogate, unassigned noncharacter):
/// what format.ts `clean` replaces with a space before it reaches the screen.
pub fn is_hidden(c: char) -> bool {
    let cp = c as u32;
    HIDDEN_RANGES.binary_search_by(|&(start, end)| {
        if cp < start { std::cmp::Ordering::Greater }
        else if cp > end { std::cmp::Ordering::Less }
        else { std::cmp::Ordering::Equal }
    }).is_ok()
}

/// Control and bidi/format characters replaced by spaces (format.ts `clean`).
pub fn clean(s: &str) -> String {
    // A hidden character becomes one space, never more bytes than it had: the input's length
    // is enough room.
    let mut out = String::with_capacity(s.len());
    out.extend(s.chars().map(|c| if is_hidden(c) { ' ' } else { c }));
    out
}

/// Truncate to a display width, ending with an ellipsis when shortened.
pub fn fit(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if width(s) <= max {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + '…'.len_utf8());
    let mut w = 0;
    for c in s.chars() {
        let cw = cell_width(c);
        if w + cw > max - 1 {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// ECMAScript `\s` and `String.prototype.trim` agree on this set: WhiteSpace plus
/// LineTerminator. Rust's `char::is_whitespace` differs (it has U+0085 and lacks U+FEFF), so
/// every port of a JavaScript whitespace rule uses this instead.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c as u32,
        0x09..=0x0d | 0x20 | 0xa0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f
            | 0x205f | 0x3000 | 0xfeff
    )
}

/// `s.split(/\s+/).filter(Boolean)`: runs of the ECMAScript whitespace set.
pub fn js_words(s: &str) -> Vec<&str> {
    s.split(is_js_whitespace).filter(|w| !w.is_empty()).collect()
}

/// Break text into lines at most `max` columns wide at spaces; a longer word breaks anywhere.
pub fn wrap(s: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in js_words(s) {
        if !line.is_empty() && width(&line) + 1 + width(word) <= max {
            line.push(' ');
            line.push_str(word);
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        let mut rest = word;
        while width(rest) > max {
            let mut head_end = 0;
            let mut w = 0;
            for (i, c) in rest.char_indices() {
                if w + cell_width(c) > max {
                    break;
                }
                w += cell_width(c);
                head_end = i + c.len_utf8();
            }
            if head_end == 0 {
                // A single character wider than the line: take it anyway.
                head_end = rest.chars().next().map_or(rest.len(), char::len_utf8);
            }
            lines.push(rest[..head_end].to_string());
            rest = &rest[head_end..];
        }
        line = rest.to_string();
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Exact power-of-two form of a finite double: `x == m * 2^e` with `m` an integer.
fn exact_parts(x: f64) -> (u64, i32) {
    let bits = x.to_bits();
    let raw = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    if raw == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1u64 << 52), raw - 1075)
    }
}

/// Whether `x * 10^digits` lands exactly halfway between two integers. Binary arithmetic in
/// [`to_fixed`] cannot see these; the exact form can: reduce `m * 2^e` to an odd `o * 2^e'`,
/// then `o * 5^d * 2^(e'+d)` is an odd half only when `e' + d == -1`.
fn exact_tie(x: f64, digits: usize) -> Option<u64> {
    let (m, e) = exact_parts(x);
    if m == 0 {
        return None;
    }
    let shift = m.trailing_zeros() as i32;
    let odd = m >> shift;
    if e + shift + digits as i32 != -1 {
        return None;
    }
    let mut scaled = odd;
    for _ in 0..digits {
        scaled = scaled.checked_mul(5)?;
    }
    // Ties round the magnitude away from zero, as V8's `toFixed` does.
    Some((scaled + 1) / 2)
}

/// Render an integer with a decimal point `digits` places from the right.
fn fixed_digits(mut value: u64, digits: usize) -> String {
    if digits == 0 {
        return value.to_string();
    }
    let mut parts = Vec::with_capacity(digits);
    for _ in 0..digits {
        parts.push(b'0' + (value % 10) as u8);
        value /= 10;
    }
    parts.reverse();
    format!("{}.{}", value, String::from_utf8(parts).unwrap())
}

/// JavaScript `Number.prototype.toFixed` for finite magnitudes: the exact binary value rounded
/// to `digits`, with exact halfway values rounded to the larger integer. From 1e21 up the
/// original returns the number's own string instead.
/// ECMAScript `Number.prototype.toFixed`, including "NaN" and "Infinity" outside its range.
pub fn to_fixed(x: f64, digits: usize) -> String {
    if !x.is_finite() {
        return js_number(x);
    }
    if x.abs() >= 1e21 {
        return js_number(x);
    }
    // `toFixed` rounds a negative number by its magnitude and keeps the sign, even when that
    // rounds to zero ("-0"). Rust's fixed-precision formatter rounds a tie to even instead.
    let negative = x < 0.0;
    let magnitude = x.abs();
    let body = if let Some(rounded) = exact_tie(magnitude, digits) {
        fixed_digits(rounded, digits)
    } else if magnitude == 0.0 {
        format!("{:.*}", digits, 0.0)
    } else {
        format!("{:.*}", digits, magnitude)
    };
    if negative { format!("-{body}") } else { body }
}

/// ECMAScript `String(number)` (Number::toString, radix 10) for any double.
pub fn js_number(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x == 0.0 {
        return "0".into(); // also -0
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    // A whole number below 2^53: every integer there is a double, so the shortest digits that
    // name it are its own, and with n <= 16 <= 21 the general path below prints them in plain
    // decimal with the sign: exactly the integer's decimal form. The labels on every frame
    // (durations, counts) are almost all such numbers.
    if x.fract() == 0.0 && x.abs() < 9_007_199_254_740_992.0 {
        return (x as i64).to_string();
    }
    js_number_general(x)
}

/// [`js_number`] for any finite, nonzero number.
fn js_number_general(x: f64) -> String {
    let negative = x < 0.0;
    let shortest = format!("{:e}", x.abs()); // shortest round-trip digits, d[.ddd]e±n
    let shortest_digits = shortest
        .split_once('e')
        .map(|(m, _)| m.chars().filter(|c| *c != '.').count())
        .unwrap_or(1);
    // Rust's shortest form and V8's agree on how many digits are needed, but not always on
    // which neighbouring decimal to print when the exact value is halfway between two of them.
    // Re-render at that same precision through Rust's correctly rounded fixed-precision path,
    // which rounds halfway values to even like ECMAScript's Number::toString.
    let s = format!("{:.*e}", shortest_digits.saturating_sub(1), x.abs());
    let (mantissa, exponent) = s.split_once('e').unwrap();
    let exponent: i32 = exponent.parse().unwrap();
    let mut digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    let k = digits.len() as i32;
    let n = exponent + 1; // value = 0.digits × 10^n
    let body = if k <= n && n <= 21 {
        format!("{}{}", digits, "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{}", "0".repeat((-n) as usize), digits)
    } else {
        let lead = if k == 1 {
            digits.clone()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!("{}e{}{}", lead, if n > 0 { "+" } else { "-" }, (n - 1).abs())
    };
    if negative { format!("-{body}") } else { body }
}

/// `String(n).padStart(2, "0")` for the small non-negative integers these formats produce.
fn pad2(n: f64) -> String {
    let s = js_number(n);
    if s.len() >= 2 { s } else { format!("0{s}") }
}

/// ECMAScript `Math.round`: halfway values go toward positive infinity, and integers are left
/// alone (adding 0.5 first would round past 2^52).
pub fn js_round(x: f64) -> f64 {
    let floor = x.floor();
    if x - floor >= 0.5 { floor + 1.0 } else { floor }
}

/// Token counts: 999, 1.2K, 123K, 1.2M, 123M, 1.2B; "--" when unknown.
pub fn tokens(n: f64) -> String {
    if !n.is_finite() {
        return "--".into();
    }
    if n >= 1e9 {
        format!("{}B", to_fixed(n / 1e9, 1))
    } else if n >= 1e6 {
        format!("{}M", to_fixed(n / 1e6, if n >= 1e8 { 0 } else { 1 }))
    } else if n >= 1e3 {
        format!("{}K", to_fixed(n / 1e3, if n >= 1e5 { 0 } else { 1 }))
    } else {
        js_number(js_round(n))
    }
}

/// Elapsed seconds: 9s, 4m 05s, 2h 07m, 3d 4h; "--" when unknown.
pub fn duration(sec: f64) -> String {
    if !sec.is_finite() {
        return "--".into();
    }
    let s = sec.max(0.0).floor();
    if s >= 86400.0 {
        format!(
            "{}d {}h",
            js_number((s / 86400.0).floor()),
            js_number(((s % 86400.0) / 3600.0).floor())
        )
    } else if s >= 3600.0 {
        format!(
            "{}h {}m",
            js_number((s / 3600.0).floor()),
            pad2(((s % 3600.0) / 60.0).floor())
        )
    } else if s >= 60.0 {
        format!(
            "{}m {}s",
            js_number((s / 60.0).floor()),
            pad2(s % 60.0)
        )
    } else {
        format!("{}s", js_number(s))
    }
}

/// `+x.toFixed(digits)`: toFixed, then `Number()` and back to a string, so "1.0" becomes "1".
pub fn fixed_number(x: f64, digits: usize) -> String {
    let fixed = to_fixed(x, digits);
    match fixed.parse::<f64>() {
        Ok(value) => js_number(value),
        Err(_) => fixed,
    }
}

/// `duration(sec).replace(/ \d+s$/, "")`: the same duration without its seconds part.
pub fn short_duration(sec: f64) -> String {
    let value = duration(sec);
    match value.rsplit_once(' ') {
        Some((head, tail))
            if tail.len() > 1
                && tail.ends_with('s')
                && tail[..tail.len() - 1].bytes().all(|b| b.is_ascii_digit()) =>
        {
            head.into()
        }
        _ => value,
    }
}

/// `duration(sec).split(" ")[0]`: only the leading unit of a duration.
pub fn coarse(sec: f64) -> String {
    duration(sec)
        .split_whitespace()
        .next()
        .unwrap_or("--")
        .into()
}

/// `view.ts`'s `spinner`: one shared beat, `SPIN[Math.floor(now / BEAT_MS) % SPIN.length]`.
pub fn spinner(now: f64) -> char {
    const SPIN: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    // Remainder in floating point, not after casting: the clock is far past 2^53 / 100.
    // A negative `now` is outside the clock's domain; JavaScript's index would be negative.
    let beat = (now / 100.0).floor() % SPIN.len() as f64;
    let index = if beat.is_finite() {
        (if beat < 0.0 { beat + SPIN.len() as f64 } else { beat }) as usize
    } else {
        0
    };
    SPIN[index]
}

/// `view.ts`'s `whole`: "1.5 on a window" → "2 on a window", by the same
/// `^(\d+\.\d+)(\D*)$` test; anything else is left alone.
pub fn whole(v: &str) -> String {
    let b = v.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 || i >= b.len() || b[i] != b'.' {
        return v.into();
    }
    i += 1;
    let fraction = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == fraction || v[i..].bytes().any(|c| c.is_ascii_digit()) {
        return v.into();
    }
    let Ok(number) = v[..i].parse::<f64>() else {
        return v.into();
    };
    if number < 1.0 {
        return v.into();
    }
    format!("{}{}", js_number(js_round(number)), &v[i..])
}

/// `label.replace(/\s*\(.*\)/, "")`: drop the first parenthesised note, with the whitespace
/// before it, up to the last closing bracket.
pub fn limit_label(label: &str) -> String {
    let chars: Vec<char> = label.chars().collect();
    let Some(open) = chars
        .iter()
        .enumerate()
        .position(|(i, &c)| c == '(' && chars[i..].contains(&')'))
    else {
        return label.into();
    };
    let close = chars.iter().rposition(|&c| c == ')').unwrap_or(open);
    let mut start = open;
    while start > 0 && is_js_whitespace(chars[start - 1]) {
        start -= 1;
    }
    let mut out: String = chars[..start].iter().collect();
    out.extend(&chars[close + 1..]);
    out
}

/// `claude-opus-4-5` → `opus 4.5`, `gpt-5` → `gpt 5`.
pub fn short_model(m: &str) -> String {
    let mut s = m.strip_prefix("claude-").unwrap_or(m).to_string();
    // -(\d+)-(\d+)$ → " $1.$2"
    let b = s.as_bytes();
    let mut i = b.len();
    while i > 0 && b[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i < b.len() && i > 0 && b[i - 1] == b'-' {
        let minor_start = i;
        let mut j = i - 1;
        while j > 0 && b[j - 1].is_ascii_digit() {
            j -= 1;
        }
        if j < i - 1 && j > 0 && b[j - 1] == b'-' {
            s = format!(
                "{} {}.{}",
                &s[..j - 1],
                &s[j..minor_start - 1],
                &s[minor_start..]
            );
        }
    }
    match s.strip_prefix("gpt-") {
        Some(rest) => format!("gpt {rest}"),
        None => s,
    }
}

// BEGIN GENERATED COLLATION (Node 26.13.1, ICU 78.3)
/// (code point, primary weight, tertiary weight) for U+0020..U+007E.
pub(crate) static COLLATION: &[(u32, u16, u8)] = &[
    (32, 0, 0),
    (33, 6, 0),
    (34, 10, 0),
    (35, 22, 0),
    (36, 32, 0),
    (37, 23, 0),
    (38, 21, 0),
    (39, 9, 0),
    (40, 11, 0),
    (41, 12, 0),
    (42, 18, 0),
    (43, 26, 0),
    (44, 3, 0),
    (45, 2, 0),
    (46, 8, 0),
    (47, 19, 0),
    (48, 33, 0),
    (49, 34, 0),
    (50, 35, 0),
    (51, 36, 0),
    (52, 37, 0),
    (53, 38, 0),
    (54, 39, 0),
    (55, 40, 0),
    (56, 41, 0),
    (57, 42, 0),
    (58, 5, 0),
    (59, 4, 0),
    (60, 27, 0),
    (61, 28, 0),
    (62, 29, 0),
    (63, 7, 0),
    (64, 17, 0),
    (65, 43, 1),
    (66, 44, 1),
    (67, 45, 1),
    (68, 46, 1),
    (69, 47, 1),
    (70, 48, 1),
    (71, 49, 1),
    (72, 50, 1),
    (73, 51, 1),
    (74, 52, 1),
    (75, 53, 1),
    (76, 54, 1),
    (77, 55, 1),
    (78, 56, 1),
    (79, 57, 1),
    (80, 58, 1),
    (81, 59, 1),
    (82, 60, 1),
    (83, 61, 1),
    (84, 62, 1),
    (85, 63, 1),
    (86, 64, 1),
    (87, 65, 1),
    (88, 66, 1),
    (89, 67, 1),
    (90, 68, 1),
    (91, 13, 0),
    (92, 20, 0),
    (93, 14, 0),
    (94, 25, 0),
    (95, 1, 0),
    (96, 24, 0),
    (97, 43, 0),
    (98, 44, 0),
    (99, 45, 0),
    (100, 46, 0),
    (101, 47, 0),
    (102, 48, 0),
    (103, 49, 0),
    (104, 50, 0),
    (105, 51, 0),
    (106, 52, 0),
    (107, 53, 0),
    (108, 54, 0),
    (109, 55, 0),
    (110, 56, 0),
    (111, 57, 0),
    (112, 58, 0),
    (113, 59, 0),
    (114, 60, 0),
    (115, 61, 0),
    (116, 62, 0),
    (117, 63, 0),
    (118, 64, 0),
    (119, 65, 0),
    (120, 66, 0),
    (121, 67, 0),
    (122, 68, 0),
    (123, 15, 0),
    (124, 30, 0),
    (125, 16, 0),
    (126, 31, 0),
];
// END GENERATED COLLATION


/// Reproduce Node's `String.prototype.localeCompare` (default locale, ICU variant weights) for
/// the strings the delegation comparator sees: ISO timestamps and ids made of printable ASCII,
/// possibly mixed case. ICU gives each such character one primary weight and one tertiary
/// (case) weight; strings compare by primary sequence, then by case sequence.
///
/// Not covered, and deliberately out of scope: non-ASCII text (any character outside
/// U+0020..U+007E), non-default locales, numeric collation, and the exact integer magnitude
/// `localeCompare` returns (only the sign is reproduced).
pub fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    fn weights(s: &str) -> Option<(Vec<u16>, Vec<u8>)> {
        let mut primary = Vec::with_capacity(s.len());
        let mut tertiary = Vec::with_capacity(s.len());
        for c in s.chars() {
            let cp = c as u32;
            if !(0x20..=0x7e).contains(&cp) {
                return None;
            }
            let (_, p, t) = COLLATION[(cp - 0x20) as usize];
            primary.push(p);
            tertiary.push(t);
        }
        Some((primary, tertiary))
    }
    match (weights(a), weights(b)) {
        (Some((pa, ta)), Some((pb, tb))) => pa
            .cmp(&pb)
            .then_with(|| ta.cmp(&tb))
            .then_with(|| a.len().cmp(&b.len())),
        // Outside the modelled alphabet the caller must use a full ICU collation instead.
        _ => a.cmp(b),
    }
}

// ── Canvas ──────────────────────────────────────────────────────────────────

/// The right half of a wide character.
pub const CONT: char = '\0';

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
}

impl Cell {
    pub fn codepoint(&self) -> u32 {
        self.ch as u32
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }
    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub const fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
    /// Shrunk by `dx` columns each side and `dy` rows top and bottom.
    pub fn inset(&self, dx: i32, dy: i32) -> Rect {
        Rect {
            x: self.x + dx,
            y: self.y + dy,
            w: (self.w - 2 * dx).max(0),
            h: (self.h - 2 * dy).max(0),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub cells: Vec<Cell>,
    // Clip rectangle (inclusive-exclusive) and the vertical offset of a scrolled panel.
    cx0: i32,
    cy0: i32,
    cx1: i32,
    cy1: i32,
    oy: i32,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Canvas {
        let (w, h) = (w.max(1), h.max(1));
        let blank = Cell {
            ch: ' ',
            fg: 0xffffff,
            bg: 0,
            bold: false,
        };
        Canvas {
            w,
            h,
            cells: vec![blank; w * h],
            cx0: 0,
            cy0: 0,
            cx1: w as i32,
            cy1: h as i32,
            oy: 0,
        }
    }

    /// [`Canvas::new`] followed by [`Canvas::clear`], with the cells written once.
    pub fn cleared(w: usize, h: usize, bg: Rgb, fg: Rgb) -> Canvas {
        let (w, h) = (w.max(1), h.max(1));
        let cell = Cell { ch: ' ', fg, bg, bold: false };
        Canvas { w, h, cells: vec![cell; w * h], cx0: 0, cy0: 0, cx1: w as i32, cy1: h as i32, oy: 0 }
    }

    pub fn resize(&mut self, w: usize, h: usize) {
        if (w.max(1), h.max(1)) != (self.w, self.h) {
            *self = Canvas::new(w, h);
        }
    }

    pub fn clear(&mut self, bg: Rgb, fg: Rgb) {
        self.cells.fill(Cell {
            ch: ' ',
            fg,
            bg,
            bold: false,
        });
        self.unclip();
    }

    pub fn unclip(&mut self) {
        (self.cx0, self.cy0, self.cx1, self.cy1, self.oy) = (0, 0, self.w as i32, self.h as i32, 0);
    }

    pub fn rect(&self) -> Rect {
        Rect::new(0, 0, self.w as i32, self.h as i32)
    }

    /// Draw inside a rectangle only, with content shifted up by `scroll` rows.
    pub fn clipped<T>(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        scroll: i32,
        f: impl FnOnce(&mut Canvas) -> T,
    ) -> T {
        let saved = (self.cx0, self.cy0, self.cx1, self.cy1, self.oy);
        // Nested clips intersect, as a clip inside a clipped panel must.
        self.cx0 = x.max(saved.0);
        self.cy0 = y.max(saved.1);
        self.cx1 = (x + w).min(saved.2);
        self.cy1 = (y + h).min(saved.3);
        self.oy = saved.4 + scroll;
        let out = f(self);
        (self.cx0, self.cy0, self.cx1, self.cy1, self.oy) = saved;
        out
    }

    fn index(&self, x: i32, y: i32) -> Option<usize> {
        let yy = y - self.oy;
        if x < self.cx0 || x >= self.cx1 || yy < self.cy0 || yy >= self.cy1 {
            return None;
        }
        Some(yy as usize * self.w + x as usize)
    }

    pub fn get(&self, x: i32, y: i32) -> Option<&Cell> {
        (x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h)
            .then(|| &self.cells[y as usize * self.w + x as usize])
    }

    pub fn set(&mut self, x: i32, y: i32, ch: char, fg: Rgb, bg: Option<Rgb>, bold: bool) {
        let Some(i) = self.index(x, y) else { return };
        // Keep wide characters whole when something is drawn over half of one.
        if ch != CONT {
            if self.cells[i].ch == CONT && x > 0 {
                self.cells[i - 1].ch = ' ';
            }
            if (x as usize) + 1 < self.w && self.cells[i + 1].ch == CONT {
                self.cells[i + 1].ch = ' ';
            }
        }
        let c = &mut self.cells[i];
        c.ch = ch;
        c.fg = fg;
        if let Some(bg) = bg {
            c.bg = bg;
        }
        c.bold = bold;
    }

    pub fn bg_at(&self, x: i32, y: i32) -> Rgb {
        self.index(x, y).map_or(0, |i| self.cells[i].bg)
    }

    /// Draw a string; returns the columns used.
    pub fn text(&mut self, x: i32, y: i32, s: &str, fg: Rgb) -> i32 {
        self.text_with(x, y, s, fg, None, false, usize::MAX)
    }
    pub fn text_bold(&mut self, x: i32, y: i32, s: &str, fg: Rgb) -> i32 {
        self.text_with(x, y, s, fg, None, true, usize::MAX)
    }
    pub fn text_bg(&mut self, x: i32, y: i32, s: &str, fg: Rgb, bg: Rgb, bold: bool) -> i32 {
        self.text_with(x, y, s, fg, Some(bg), bold, usize::MAX)
    }
    /// The TypeScript `text(x, y, s, fg, { bg, attr, max })`: stops before a character that
    /// would pass `max` columns.
    pub fn text_with(
        &mut self,
        x: i32,
        y: i32,
        s: &str,
        fg: Rgb,
        bg: Option<Rgb>,
        bold: bool,
        max: usize,
    ) -> i32 {
        let mut col = 0usize;
        for c in s.chars() {
            let c = if is_hidden(c) { ' ' } else { c };
            let cw = cell_width(c);
            if cw == 0 {
                continue;
            }
            if col + cw > max {
                break;
            }
            self.set(x + col as i32, y, c, fg, bg, bold);
            if cw == 2 {
                self.set(x + col as i32 + 1, y, CONT, fg, bg, bold);
            }
            col += cw;
        }
        col as i32
    }

    /// Right-align text so it ends at column `right` (exclusive).
    pub fn right(&mut self, right: i32, y: i32, s: &str, fg: Rgb) -> i32 {
        self.right_with(right, y, s, fg, None, false)
    }
    pub fn right_with(
        &mut self,
        right: i32,
        y: i32,
        s: &str,
        fg: Rgb,
        bg: Option<Rgb>,
        bold: bool,
    ) -> i32 {
        let w = width(&clean(s)) as i32;
        self.text_with(right - w, y, s, fg, bg, bold, usize::MAX)
    }

    /// Right-align text that may use at most `max` columns, as `textRight(..., { max })`:
    /// the row stops at `max` columns with no ellipsis.
    pub fn right_max(
        &mut self,
        right: i32,
        y: i32,
        s: &str,
        fg: Rgb,
        bg: Option<Rgb>,
        bold: bool,
        max: usize,
    ) -> i32 {
        let w = (width(&clean(s)) as usize).min(max) as i32;
        self.text_with(right - w, y, s, fg, bg, bold, max)
    }

    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, bg: Rgb) {
        self.fill_with(x, y, w, h, bg, ' ', bg);
    }
    pub fn fill_with(&mut self, x: i32, y: i32, w: i32, h: i32, bg: Rgb, ch: char, fg: Rgb) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.set(xx, yy, ch, fg, Some(bg), false);
            }
        }
    }

    /// Recolor a region in place (modal dimming).
    pub fn tint(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        mut f: impl FnMut(Rgb, Rgb) -> (Rgb, Rgb),
    ) {
        for yy in y..y + h {
            for xx in x..x + w {
                if let Some(i) = self.index(xx, yy) {
                    let c = &mut self.cells[i];
                    (c.fg, c.bg) = f(c.fg, c.bg);
                }
            }
        }
    }

    pub fn row_text(&self, y: usize) -> String {
        let row = &self.cells[y * self.w..(y + 1) * self.w];
        let s: String = row.iter().filter(|c| c.ch != CONT).map(|c| c.ch).collect();
        s.trim_end().to_string()
    }

    pub fn to_text(&self) -> String {
        (0..self.h)
            .map(|y| self.row_text(y))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ── Glyph primitives ────────────────────────────────────────────────────────

const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Section header: bold label, a rule to the right, optional right-aligned note.
pub fn section(c: &mut Canvas, x: i32, y: i32, w: i32, label: &str, p: &Palette, right: &str) {
    section_with(c, x, y, w, label, p, right, p.dim);
}
pub fn section_with(
    c: &mut Canvas,
    x: i32,
    y: i32,
    w: i32,
    label: &str,
    p: &Palette,
    right: &str,
    right_color: Rgb,
) {
    let n = c.text_bold(x, y, label, p.text2);
    let rw = if right.is_empty() {
        0
    } else {
        width(right) as i32 + 1
    };
    for i in x + n + 1..x + w - rw - 1 {
        c.set(i, y, '─', p.rule, None, false);
    }
    if !right.is_empty() {
        c.right(x + w, y, right, right_color);
    }
}

pub struct BarOpts<'a> {
    pub track: Rgb,
    pub color: &'a dyn Fn(usize, usize) -> Rgb,
    /// A tick on the track (fraction, color), e.g. elapsed time.
    pub marker: Option<(f64, Rgb)>,
    /// Draw the empty track as a thin rule instead of a filled background.
    pub line: bool,
}

/// Horizontal meter with 1/8-cell precision; the track is the background.
pub fn hbar(c: &mut Canvas, x: i32, y: i32, w: i32, frac: f64, fg: Rgb, track: Rgb) {
    hbar_with(
        c,
        x,
        y,
        w,
        frac,
        &BarOpts {
            track,
            color: &|_, _| fg,
            marker: None,
            line: false,
        },
    );
}

pub fn hbar_with(c: &mut Canvas, x: i32, y: i32, w: i32, frac: f64, o: &BarOpts) {
    if w <= 0 {
        return;
    }
    let units = (frac.clamp(0.0, 1.0) * w as f64 * 8.0 + 0.5).floor() as i32;
    let (full, rem) = (units / 8, units % 8);
    let bg = if o.line { None } else { Some(o.track) };
    let wu = w as usize;
    for i in 0..w {
        if i < full {
            c.set(x + i, y, '█', (o.color)(i as usize, wu), bg, false);
        } else if i == full && rem > 0 {
            c.set(
                x + i,
                y,
                EIGHTHS[rem as usize],
                (o.color)(i as usize, wu),
                bg,
                false,
            );
        } else {
            c.set(x + i, y, if o.line { '─' } else { ' ' }, o.track, bg, false);
        }
    }
    if let Some((at, mc)) = o.marker {
        let mx = ((at * w as f64 + 0.5).floor() as i32).clamp(0, w - 1);
        let filled = mx < full;
        c.set(
            x + mx,
            y,
            if filled { '▕' } else { '│' },
            mc,
            if filled {
                Some((o.color)(mx as usize, wu))
            } else {
                bg
            },
            false,
        );
    }
}

/// One of eight levels for `v` of `max`; a space for zero.
pub fn level(v: f64, max: f64) -> char {
    if v <= 0.0 {
        return ' ';
    }
    let i = ((v / max.max(1.0)) * 8.0).ceil() as i64 - 1;
    LEVELS[i.clamp(0, 7) as usize]
}

/// Sparkline: one column per value.
pub fn spark(values: &[f64], max: Option<f64>) -> String {
    let max = max.unwrap_or_else(|| values.iter().copied().fold(1.0, f64::max));
    values
        .iter()
        .map(|&v| {
            if v <= 0.0 {
                ' '
            } else {
                LEVELS[(((v / max) * 8.0).ceil() as i64 - 1).clamp(0, 7) as usize]
            }
        })
        .collect()
}

/// 2×4 braille dot raster for line and area charts.
pub struct Braille {
    pub w: usize,
    pub h: usize,
    bits: Vec<u8>,
}

impl Braille {
    const MAP: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

    pub fn new(cols: usize, rows: usize) -> Braille {
        Braille {
            w: cols * 2,
            h: rows * 4,
            bits: vec![0; cols * rows],
        }
    }
    pub fn dot(&mut self, px: i32, py: i32) {
        if px < 0 || py < 0 || px as usize >= self.w || py as usize >= self.h {
            return;
        }
        let (px, py) = (px as usize, py as usize);
        self.bits[(py / 4) * (self.w / 2) + px / 2] |= Self::MAP[py % 4][px % 2];
    }
    /// Fill a column from the bottom up to a 0..1 height.
    pub fn column(&mut self, px: i32, frac: f64) {
        let top = self.h as i32 - (frac.clamp(0.0, 1.0) * self.h as f64 + 0.5).floor() as i32;
        let mut py = self.h as i32 - 1;
        while py >= top {
            self.dot(px, py);
            py -= 1;
        }
    }
    pub fn draw(
        &self,
        c: &mut Canvas,
        x: i32,
        y: i32,
        color: impl Fn(usize, usize) -> Rgb,
        bg: Option<Rgb>,
    ) {
        let cols = self.w / 2;
        for row in 0..self.h / 4 {
            for col in 0..cols {
                let b = self.bits[row * cols + col];
                if b != 0 {
                    let ch = char::from_u32(0x2800 + b as u32).unwrap_or(' ');
                    c.set(
                        x + col as i32,
                        y + row as i32,
                        ch,
                        color(col, row),
                        bg,
                        false,
                    );
                }
            }
        }
    }
}

// ── Big numbers (canvas.ts faces) ───────────────────────────────────────────
// "plain" (large = false): a 4×5 pixel font in half blocks, three rows tall.
// "shadow" (large = true): the cfonts "block" digits six rows tall, each block casting a
// box-drawing shadow drawn in the shade color. Digits are tabular within a face; units
// (% K M B) are bold text on the face's baseline.

const PLAIN_GLYPHS: [(char, &str); 12] = [
    ('0', ".##./#..#/#..#/#..#/.##."),
    ('1', ".#../##../.#../.#../###."),
    ('2', "###./...#/.##./#.../####"),
    ('3', "###./...#/.##./...#/###."),
    ('4', "#..#/#..#/####/...#/...#"),
    ('5', "####/#.../###./...#/###."),
    ('6', ".##./#.../###./#..#/.##."),
    ('7', "####/...#/..#./.#../.#.."),
    ('8', ".##./#..#/.##./#..#/.##."),
    ('9', ".##./#..#/.###/...#/.##."),
    ('.', "././././#"),
    ('-', ".../.../###/.../..."),
];

// OpenTUI's bundled cfonts "block" digits, markup stripped, with 0, 1 and 6 redrawn eight
// wide as canvas.ts SHADOW_OWN does.
const SHADOW_GLYPHS: [(char, [&str; 6]); 12] = [
    (
        '0',
        [
            " █████╗ ",
            "██╔══██╗",
            "██║  ██║",
            "██║  ██║",
            "╚█████╔╝",
            " ╚════╝ ",
        ],
    ),
    (
        '1',
        [
            "  ██╗   ",
            " ███║   ",
            " ╚██║   ",
            "  ██║   ",
            "██████╗ ",
            "╚═════╝ ",
        ],
    ),
    (
        '2',
        [
            "██████╗ ",
            "╚════██╗",
            " █████╔╝",
            "██╔═══╝ ",
            "███████╗",
            "╚══════╝",
        ],
    ),
    (
        '3',
        [
            "██████╗ ",
            "╚════██╗",
            " █████╔╝",
            " ╚═══██╗",
            "██████╔╝",
            "╚═════╝ ",
        ],
    ),
    (
        '4',
        [
            "██╗  ██╗",
            "██║  ██║",
            "███████║",
            "╚════██║",
            "     ██║",
            "     ╚═╝",
        ],
    ),
    (
        '5',
        [
            "███████╗",
            "██╔════╝",
            "███████╗",
            "╚════██║",
            "███████║",
            "╚══════╝",
        ],
    ),
    (
        '6',
        [
            " █████╗ ",
            "██╔═══╝ ",
            "██████╗ ",
            "██╔══██╗",
            "╚█████╔╝",
            " ╚════╝ ",
        ],
    ),
    (
        '7',
        [
            "███████╗",
            "╚════██║",
            "    ██╔╝",
            "   ██╔╝ ",
            "   ██║  ",
            "   ╚═╝  ",
        ],
    ),
    (
        '8',
        [
            " █████╗ ",
            "██╔══██╗",
            "╚█████╔╝",
            "██╔══██╗",
            "╚█████╔╝",
            " ╚════╝ ",
        ],
    ),
    (
        '9',
        [
            " █████╗ ",
            "██╔══██╗",
            "╚██████║",
            " ╚═══██║",
            " █████╔╝",
            " ╚════╝ ",
        ],
    ),
    ('.', ["   ", "   ", "   ", "   ", "██╗", "╚═╝"]),
    (
        '-',
        ["      ", "      ", "█████╗", "╚════╝", "      ", "      "],
    ),
];

/// Rows of a big number: 6 for the shadow face, 3 for plain.
pub fn big_rows(large: bool) -> i32 {
    if large { 6 } else { 3 }
}
/// The row units sit on.
pub fn big_base(large: bool) -> i32 {
    if large { 4 } else { 2 }
}

fn half_blocks(def: &str) -> Vec<String> {
    let rows: Vec<&[u8]> = def.split('/').map(str::as_bytes).collect();
    let cols = rows[0].len();
    // Pixel row -1 is the top gap.
    let lit = |row: i32, col: usize| {
        row >= 1 && rows.get(row as usize - 1).and_then(|r| r.get(col)) == Some(&b'#')
    };
    (0..3)
        .map(|r| {
            (0..cols)
                .map(|col| match (lit(r * 2, col), lit(r * 2 + 1, col)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    _ => ' ',
                })
                .collect()
        })
        .collect()
}

thread_local! {
    static PLAIN: Vec<(char, Vec<String>)> = PLAIN_GLYPHS.iter().map(|&(c, d)| (c, half_blocks(d))).collect();
}

/// A big glyph's rows, or None for a character drawn as text.
pub fn big_glyph(ch: char, large: bool) -> Option<Vec<String>> {
    if large {
        SHADOW_GLYPHS
            .iter()
            .find(|g| g.0 == ch)
            .map(|g| g.1.iter().map(|s| s.to_string()).collect())
    } else {
        PLAIN.with(|p| p.iter().find(|g| g.0 == ch).map(|g| g.1.clone()))
    }
}

fn glyph_cols(ch: char, large: bool) -> Option<usize> {
    if large {
        SHADOW_GLYPHS
            .iter()
            .find(|g| g.0 == ch)
            .map(|g| g.1[0].chars().count())
    } else {
        PLAIN_GLYPHS
            .iter()
            .find(|g| g.0 == ch)
            .map(|g| g.1.split('/').next().map_or(0, str::len))
    }
}

/// Width of a big number; characters without a glyph are one text cell.
pub fn big_width(s: &str, large: bool) -> i32 {
    let w: usize = s
        .chars()
        .map(|c| glyph_cols(c, large).unwrap_or_else(|| cell_width(c)) + 1)
        .sum();
    w.saturating_sub(1) as i32
}

/// Draw a big number; units are bold text on the baseline and the shadow strokes take `shadow`
/// (canvas.ts uses `mix(color, 0, 0.5)` by default). Returns the columns used.
pub fn draw_big(c: &mut Canvas, x: i32, y: i32, s: &str, fg: Rgb, shadow: Rgb, large: bool) -> i32 {
    let mut cx = x;
    for ch in s.chars() {
        let Some(rows) = big_glyph(ch, large) else {
            cx += c.text_bold(cx, y + big_base(large), &ch.to_string(), fg) + 1;
            continue;
        };
        let mut gw = 0;
        for (ry, row) in rows.iter().enumerate() {
            for (col, g) in row.chars().enumerate() {
                if g != ' ' {
                    let shade = ('\u{2500}'..='\u{257f}').contains(&g);
                    c.set(
                        cx + col as i32,
                        y + ry as i32,
                        g,
                        if shade { shadow } else { fg },
                        None,
                        false,
                    );
                }
                gw = gw.max(col as i32 + 1);
            }
        }
        cx += gw + 1;
    }
    (cx - x - 1).max(0)
}

// ── Rendering ───────────────────────────────────────────────────────────────

/// Code points the embedded font covers below U+E000 (fc-query charset, identical for Regular
/// and Bold), inclusive ranges.
const COVERED: &[(u32, u32)] = &[
    (0x20, 0x7e),
    (0xa0, 0x131),
    (0x134, 0x17f),
    (0x18f, 0x190),
    (0x192, 0x192),
    (0x19b, 0x19b),
    (0x1a0, 0x1a1),
    (0x1af, 0x1b0),
    (0x1cd, 0x1ce),
    (0x1d0, 0x1d0),
    (0x1d2, 0x1d2),
    (0x1d4, 0x1d4),
    (0x1d6, 0x1d6),
    (0x1d8, 0x1d8),
    (0x1da, 0x1da),
    (0x1dc, 0x1dc),
    (0x1e6, 0x1e7),
    (0x1ea, 0x1eb),
    (0x1f4, 0x1f5),
    (0x1fc, 0x1ff),
    (0x218, 0x21b),
    (0x232, 0x233),
    (0x237, 0x237),
    (0x259, 0x259),
    (0x2b9, 0x2ba),
    (0x2bc, 0x2bc),
    (0x2c6, 0x2c7),
    (0x2c9, 0x2c9),
    (0x2d8, 0x2dd),
    (0x2f3, 0x2f3),
    (0x2f7, 0x2f7),
    (0x300, 0x304),
    (0x306, 0x30c),
    (0x30f, 0x30f),
    (0x312, 0x312),
    (0x31b, 0x31b),
    (0x323, 0x323),
    (0x325, 0x328),
    (0x336, 0x338),
    (0x374, 0x375),
    (0x37e, 0x37e),
    (0x384, 0x38a),
    (0x38c, 0x38c),
    (0x38e, 0x3a1),
    (0x3a3, 0x3cf),
    (0x3d5, 0x3d7),
    (0x401, 0x40c),
    (0x40e, 0x44f),
    (0x451, 0x45c),
    (0x45e, 0x45f),
    (0x490, 0x493),
    (0x49a, 0x49b),
    (0x4a2, 0x4a3),
    (0x4ae, 0x4b1),
    (0x4b6, 0x4b7),
    (0x4ba, 0x4bb),
    (0x4d8, 0x4d9),
    (0x4dc, 0x4df),
    (0x4e4, 0x4e9),
    (0x4f4, 0x4f5),
    (0xaea, 0xaea),
    (0x1e80, 0x1e85),
    (0x1e9e, 0x1e9e),
    (0x1ea0, 0x1ef9),
    (0x2001, 0x2001),
    (0x200b, 0x200b),
    (0x2010, 0x2010),
    (0x2013, 0x2014),
    (0x2016, 0x2016),
    (0x2018, 0x2022),
    (0x2024, 0x2024),
    (0x2026, 0x2026),
    (0x2030, 0x2030),
    (0x2032, 0x2034),
    (0x2039, 0x203a),
    (0x203c, 0x203f),
    (0x2044, 0x2046),
    (0x2070, 0x2070),
    (0x2074, 0x207a),
    (0x207c, 0x207c),
    (0x2080, 0x2089),
    (0x20ab, 0x20ac),
    (0x20ae, 0x20ae),
    (0x20b4, 0x20b4),
    (0x20bd, 0x20bd),
    (0x20bf, 0x20bf),
    (0x2102, 0x2102),
    (0x210d, 0x210d),
    (0x2113, 0x2113),
    (0x2115, 0x2116),
    (0x2119, 0x211a),
    (0x211d, 0x211d),
    (0x2122, 0x2122),
    (0x2124, 0x2124),
    (0x212e, 0x212e),
    (0x2140, 0x2140),
    (0x218a, 0x218b),
    (0x2190, 0x2199),
    (0x219d, 0x219e),
    (0x21a0, 0x21a0),
    (0x21a2, 0x21a3),
    (0x21a5, 0x21a7),
    (0x21a9, 0x21aa),
    (0x21ad, 0x21ad),
    (0x21be, 0x21be),
    (0x21c9, 0x21c9),
    (0x21d0, 0x21d4),
    (0x21db, 0x21db),
    (0x21de, 0x21df),
    (0x21e5, 0x21e5),
    (0x21e7, 0x21e8),
    (0x21ea, 0x21ea),
    (0x2200, 0x220c),
    (0x220e, 0x2213),
    (0x2215, 0x2215),
    (0x2218, 0x221a),
    (0x221e, 0x221e),
    (0x2223, 0x2225),
    (0x2227, 0x222b),
    (0x2234, 0x2239),
    (0x223c, 0x223c),
    (0x223e, 0x223e),
    (0x2243, 0x2243),
    (0x2245, 0x2245),
    (0x2247, 0x2249),
    (0x224b, 0x224b),
    (0x224d, 0x224d),
    (0x2254, 0x2254),
    (0x2257, 0x2257),
    (0x225f, 0x2265),
    (0x226a, 0x2273),
    (0x227a, 0x227c),
    (0x2282, 0x2289),
    (0x228e, 0x2299),
    (0x229b, 0x22a5),
    (0x22b4, 0x22b4),
    (0x22b8, 0x22b8),
    (0x22bb, 0x22bd),
    (0x22c2, 0x22c4),
    (0x22c6, 0x22c6),
    (0x22c8, 0x22ca),
    (0x22ce, 0x22ce),
    (0x22d0, 0x22d0),
    (0x22e2, 0x22e2),
    (0x22ee, 0x22f1),
    (0x2302, 0x2305),
    (0x2308, 0x230b),
    (0x2318, 0x2318),
    (0x231c, 0x231f),
    (0x2324, 0x2326),
    (0x2328, 0x2328),
    (0x232b, 0x232b),
    (0x2336, 0x237a),
    (0x2389, 0x238b),
    (0x2395, 0x2395),
    (0x239b, 0x23ad),
    (0x23ce, 0x23ce),
    (0x23fb, 0x23fe),
    (0x2400, 0x2421),
    (0x2423, 0x2424),
    (0x2500, 0x25a1),
    (0x25aa, 0x25ab),
    (0x25b2, 0x25cc),
    (0x25ce, 0x25cf),
    (0x25d4, 0x25d5),
    (0x25e6, 0x25eb),
    (0x25ef, 0x25ef),
    (0x25f6, 0x25f6),
    (0x2630, 0x2630),
    (0x2665, 0x2665),
    (0x266d, 0x266d),
    (0x266f, 0x266f),
    (0x2687, 0x2687),
    (0x26a0, 0x26a1),
    (0x2713, 0x2713),
    (0x2715, 0x2715),
    (0x2717, 0x2717),
    (0x2736, 0x2736),
    (0x276c, 0x2771),
    (0x2794, 0x2794),
    (0x279c, 0x279e),
    (0x27c5, 0x27c6),
    (0x27dc, 0x27dc),
    (0x27e6, 0x27eb),
    (0x27f5, 0x27f7),
    (0x2800, 0x28ff),
    (0x2916, 0x2916),
    (0x294a, 0x294a),
    (0x2987, 0x2988),
    (0x2a00, 0x2a00),
    (0x2a05, 0x2a06),
    (0x2b58, 0x2b58),
];

pub fn font_covers(c: char) -> bool {
    let cp = c as u32;
    COVERED
        .binary_search_by(|&(a, b)| {
            if cp < a {
                std::cmp::Ordering::Greater
            } else if cp > b {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// How a cell's character is painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paint {
    /// Nothing visible (space, continuation).
    Blank,
    /// A glyph of the shaped row line: covered by the font at exactly one cell's advance.
    Line,
    /// Rectangles in the row's vector layer: blocks, box drawing, braille.
    Vector,
    /// Its own text at its column, `n` columns wide: wide, symbol and fallback glyphs that
    /// must not move the columns after them.
    Pinned(u16),
}

pub fn paint_of(c: char) -> Paint {
    if c == ' ' || c == CONT || c == '\u{a0}' {
        return Paint::Blank;
    }
    let cp = c as u32;
    if is_vector(cp) {
        static FONT_GRAPHICS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *FONT_GRAPHICS
            .get_or_init(|| std::env::var("T3_RUNE_GRAPHICS").as_deref() != Ok("vector"))
            && font_covers(c)
        {
            return Paint::Line;
        }
        return Paint::Vector;
    }
    let w = cell_width(c);
    if w == 0 {
        return Paint::Blank;
    }
    if w == 1 && font_covers(c) && (cp < 0x2300 || (0x2400..0x2600).contains(&cp)) {
        return Paint::Line;
    }
    Paint::Pinned(w as u16)
}

pub fn is_vector(cp: u32) -> bool {
    (0x2580..=0x259f).contains(&cp)
        || (0x2800..=0x28ff).contains(&cp)
        || box_arms(cp).is_some()
        || box_special(cp)
}

/// Arms (up, right, down, left) of a box-drawing character: 1 light, 2 heavy, 3 double.
pub fn box_arms(cp: u32) -> Option<[u8; 4]> {
    const LH: [[u8; 4]; 76] = [
        [0, 1, 0, 1],
        [0, 2, 0, 2],
        [1, 0, 1, 0],
        [2, 0, 2, 0], // ─━│┃
        [0; 4],
        [0; 4],
        [0; 4],
        [0; 4],
        [0; 4],
        [0; 4],
        [0; 4],
        [0; 4], // ┄┅┆┇┈┉┊┋ (dashes)
        [0, 1, 1, 0],
        [0, 2, 1, 0],
        [0, 1, 2, 0],
        [0, 2, 2, 0], // ┌┍┎┏
        [0, 0, 1, 1],
        [0, 0, 1, 2],
        [0, 0, 2, 1],
        [0, 0, 2, 2], // ┐┑┒┓
        [1, 1, 0, 0],
        [1, 2, 0, 0],
        [2, 1, 0, 0],
        [2, 2, 0, 0], // └┕┖┗
        [1, 0, 0, 1],
        [1, 0, 0, 2],
        [2, 0, 0, 1],
        [2, 0, 0, 2], // ┘┙┚┛
        [1, 1, 1, 0],
        [1, 2, 1, 0],
        [2, 1, 1, 0],
        [1, 1, 2, 0], // ├┝┞┟
        [2, 1, 2, 0],
        [2, 2, 1, 0],
        [1, 2, 2, 0],
        [2, 2, 2, 0], // ┠┡┢┣
        [1, 0, 1, 1],
        [1, 0, 1, 2],
        [2, 0, 1, 1],
        [1, 0, 2, 1], // ┤┥┦┧
        [2, 0, 2, 1],
        [2, 0, 1, 2],
        [1, 0, 2, 2],
        [2, 0, 2, 2], // ┨┩┪┫
        [0, 1, 1, 1],
        [0, 1, 1, 2],
        [0, 2, 1, 1],
        [0, 2, 1, 2], // ┬┭┮┯
        [0, 1, 2, 1],
        [0, 1, 2, 2],
        [0, 2, 2, 1],
        [0, 2, 2, 2], // ┰┱┲┳
        [1, 1, 0, 1],
        [1, 1, 0, 2],
        [1, 2, 0, 1],
        [1, 2, 0, 2], // ┴┵┶┷
        [2, 1, 0, 1],
        [2, 1, 0, 2],
        [2, 2, 0, 1],
        [2, 2, 0, 2], // ┸┹┺┻
        [1, 1, 1, 1],
        [1, 1, 1, 2],
        [1, 2, 1, 1],
        [1, 2, 1, 2], // ┼┽┾┿
        [2, 1, 1, 1],
        [1, 1, 2, 1],
        [2, 1, 2, 1],
        [2, 1, 1, 2], // ╀╁╂╃
        [2, 2, 1, 1],
        [1, 1, 2, 2],
        [1, 2, 2, 1],
        [2, 2, 1, 2], // ╄╅╆╇
        [1, 2, 2, 2],
        [2, 1, 2, 2],
        [2, 2, 2, 1],
        [2, 2, 2, 2], // ╈╉╊╋
    ];
    match cp {
        0x2500..=0x254b => Some(LH[(cp - 0x2500) as usize]).filter(|a| a.iter().any(|&w| w > 0)),
        0x2550 => Some([0, 3, 0, 3]), // ═
        0x2551 => Some([3, 0, 3, 0]), // ║
        0x2554 => Some([0, 3, 3, 0]), // ╔
        0x2557 => Some([0, 0, 3, 3]), // ╗
        0x255a => Some([3, 3, 0, 0]), // ╚
        0x255d => Some([3, 0, 0, 3]), // ╝
        0x2560 => Some([3, 3, 3, 0]), // ╠
        0x2563 => Some([3, 0, 3, 3]), // ╣
        0x2566 => Some([0, 3, 3, 3]), // ╦
        0x2569 => Some([3, 3, 0, 3]), // ╩
        0x256c => Some([3, 3, 3, 3]), // ╬
        0x2574 => Some([0, 0, 0, 1]),
        0x2575 => Some([1, 0, 0, 0]),
        0x2576 => Some([0, 1, 0, 0]),
        0x2577 => Some([0, 0, 1, 0]),
        0x2578 => Some([0, 0, 0, 2]),
        0x2579 => Some([2, 0, 0, 0]),
        0x257a => Some([0, 2, 0, 0]),
        0x257b => Some([0, 0, 2, 0]),
        0x257c => Some([0, 2, 0, 1]),
        0x257d => Some([1, 0, 2, 0]),
        0x257e => Some([0, 1, 0, 2]),
        0x257f => Some([2, 0, 1, 0]),
        _ => None,
    }
}

/// Dashed lines and rounded corners, drawn by `push_vector` without an arms entry.
pub fn box_special(cp: u32) -> bool {
    matches!(cp, 0x2504..=0x250b | 0x254c..=0x254f | 0x256d..=0x2570)
}

/// One row of a canvas, planned for painting.
#[derive(Debug, Default, PartialEq)]
pub struct RowPlan {
    /// The narrow-cell line: a character per column, a space for blank, vector and pinned cells.
    pub line: String,
    /// Byte ranges of `line` with their (fg, bold), adjacent same-styled runs coalesced.
    pub spans: Vec<(usize, usize, Rgb, bool)>,
    /// Background runs (start col, end col, bg) that differ from the canvas base.
    pub fills: Vec<(u16, u16, Rgb)>,
    /// Pinned cells: (col, width, char, fg, bold).
    pub pinned: Vec<(u16, u16, char, Rgb, bool)>,
    /// Columns drawn in the vector layer.
    pub vectors: Vec<u16>,
}

/// The most common background, painted once under the whole grid.
pub fn base_bg(c: &Canvas) -> Rgb {
    let mut counts: HashMap<Rgb, usize> = HashMap::new();
    let mut prev = None;
    let mut run = 0usize;
    for cell in &c.cells {
        if Some(cell.bg) == prev {
            run += 1;
        } else {
            if let Some(p) = prev {
                *counts.entry(p).or_default() += run;
            }
            prev = Some(cell.bg);
            run = 1;
        }
    }
    if let Some(p) = prev {
        *counts.entry(p).or_default() += run;
    }
    counts
        .into_iter()
        .max_by_key(|&(bg, n)| (n, bg))
        .map_or(0, |(bg, _)| bg)
}

pub fn plan_row(c: &Canvas, y: usize, base: Rgb) -> RowPlan {
    let row = &c.cells[y * c.w..(y + 1) * c.w];
    let mut p = RowPlan {
        line: String::with_capacity(c.w),
        ..Default::default()
    };
    // Pending span: start byte, fg, bold; and the end byte of its last visible glyph.
    let mut span: Option<(usize, Rgb, bool, usize)> = None;
    let mut fill: Option<(u16, Rgb)> = None;
    for (x, cell) in row.iter().enumerate() {
        let col = x as u16;
        if fill.is_some_and(|(_, bg)| bg != cell.bg) {
            let (start, bg) = fill.take().unwrap();
            if bg != base {
                p.fills.push((start, col, bg));
            }
        }
        if fill.is_none() {
            fill = Some((col, cell.bg));
        }
        let at = p.line.len();
        match paint_of(cell.ch) {
            Paint::Line => {
                p.line.push(cell.ch);
                let end = p.line.len();
                match span {
                    Some((s, fg, b, _)) if fg == cell.fg && b == cell.bold => {
                        span = Some((s, fg, b, end))
                    }
                    _ => {
                        if let Some((s, fg, b, e)) = span {
                            p.spans.push((s, e, fg, b));
                        }
                        span = Some((at, cell.fg, cell.bold, end));
                    }
                }
            }
            paint => {
                // Spaces take any style, so a styled run continues across them.
                p.line.push(' ');
                match paint {
                    Paint::Vector => p.vectors.push(col),
                    Paint::Pinned(w) => {
                        p.pinned
                            .push((col, w.min((c.w - x) as u16), cell.ch, cell.fg, cell.bold))
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some((start, bg)) = fill
        && bg != base
    {
        p.fills.push((start, c.w as u16, bg));
    }
    if let Some((s, fg, b, e)) = span {
        p.spans.push((s, e, fg, b));
    }
    p
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[test]
    fn js_number_whole_numbers_take_the_general_paths_answer() {
        let mut seeds = vec![1., 2., 7., 9., 10., 59., 60., 99., 100., 1e3, 86_399., 1e15, 1e16 - 1.];
        seeds.extend([9_007_199_254_740_991., 4_503_599_627_370_496., 123_456_789_012_345.]);
        let mut rng: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..20_000 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let bits = (rng % 53) as u32;
            seeds.push((rng >> 11 & ((1u64 << bits) - 1).max(1)) as f64);
        }
        for x in seeds {
            for v in [x, -x] {
                if v != 0. {
                    assert_eq!(js_number(v), js_number_general(v), "{v}");
                }
            }
        }
        assert_eq!(js_number(-0.), "0");
        assert_eq!(js_number(1.5), "1.5");
        assert_eq!(js_number(9_007_199_254_740_992.), "9007199254740992");
        assert_eq!(js_number(2f64.powi(60)), "1152921504606847000");
    }

    #[test]
    fn family_matches_case_insensitively_on_every_path() {
        use ProviderFamily::*;
        let long = format!("{}SONNET", "x".repeat(70));
        for (model, hint, want) in [
            ("Claude-Opus-4", None, Claude),
            ("GPT-5.1-CODEX", None, OpenAi),
            ("O3", None, OpenAi),
            ("pro3", None, Other),
            ("Grok-4", None, Grok),
            ("unknown", Some("OpenAI"), OpenAi),
            (long.as_str(), None, Claude),
            ("ÉCLAUDE", None, Claude),
            ("Ω-GROK", None, Grok),
            ("", None, Other),
        ] {
            assert_eq!(family(model, hint), want, "{model}");
        }
    }

    #[test]
    fn cleared_is_new_then_clear() {
        for (w, h) in [(0, 0), (1, 1), (190, 60), (7, 3)] {
            let mut a = Canvas::new(w, h);
            (a.cx0, a.cy0, a.cx1, a.cy1, a.oy) = (1, 1, 2, 2, 3);
            a.clear(0x123456, 0xabcdef);
            let b = Canvas::cleared(w, h, 0x123456, 0xabcdef);
            assert_eq!((a.w, a.h, &a.cells), (b.w, b.h, &b.cells));
            assert_eq!((a.cx0, a.cy0, a.cx1, a.cy1, a.oy), (b.cx0, b.cy0, b.cx1, b.cy1, b.oy));
        }
    }
    use super::*;

    /// The original's own values (`ramp(p, base + span * (i / cells))` in `oracle/src/theme.ts`,
    /// TokyoNight) at positions where dividing last gives another color. If these fail, someone
    /// regrouped [`along`].
    #[test]
    fn a_position_along_a_bar_divides_first_as_the_original_does() {
        let p = Palette::tokyo();
        for (base, span, i, cells, original) in [
            (0.3, 0.7, 45., 72., 0xa5bef8),
            (0.3, 0.7, 85., 136., 0xa5bef8),
            (0.3, 0.7, 45., 192., 0x7398e7),
            (0.3, 0.7, 187., 268., 0xb0c5f9),
        ] {
            assert_eq!(ramp(&p, along(base, span, i, cells)), original, "{i} of {cells}");
            // The other grouping really is another color here, so this test can fail.
            assert_ne!(ramp(&p, base + span * i / cells), original, "{i} of {cells}");
        }
    }

    #[test]
    fn widths_match_format_ts() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("日本"), 4);
        assert_eq!(width("e\u{301}"), 1);
        assert_eq!(width("🙂"), 2);
        assert_eq!(width("─█⠿◆"), 4);
        assert_eq!(fit("hello world", 5), "hell…");
        assert_eq!(fit("hi", 5), "hi");
        assert_eq!(fit("日本語", 4), "日…");
        assert_eq!(fit("x", 0), "");
        assert_eq!(wrap("aa bb cc", 5), vec!["aa bb", "cc"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn formats_match_format_ts() {
        assert_eq!(tokens(999.4), "999");
        assert_eq!(tokens(1250.0), "1.3K"); // toFixed rounds an exact tie up
        assert_eq!(tokens(123_456.0), "123K");
        assert_eq!(tokens(18_500_000.0), "18.5M");
        assert_eq!(tokens(150_000_000.0), "150M");
        assert_eq!(tokens(2.5e9), "2.5B");
        assert_eq!(tokens(f64::NAN), "--");
        assert_eq!(duration(9.9), "9s");
        assert_eq!(duration(245.0), "4m 05s");
        assert_eq!(duration(7620.0), "2h 07m");
        assert_eq!(duration(3.0 * 86400.0 + 4.0 * 3600.0), "3d 4h");
        assert_eq!(short_model("claude-opus-4-5"), "opus 4.5");
        assert_eq!(short_model("claude-opus-5-5"), "opus 5.5");
        assert_eq!(short_model("gpt-5"), "gpt 5");
        assert_eq!(short_model("grok-4"), "grok-4");
        assert_eq!(mix(0x000000, 0xffffff, 0.5), 0x808080);
        assert_eq!(ramp(&Palette::tokyo(), 0.0), 0x4a5f8f);
        assert_eq!(ramp(&Palette::tokyo(), 1.0), 0xe0e5fa);
        assert_eq!(family("o3-mini", None), ProviderFamily::OpenAi);
        assert_eq!(family("proto3", None), ProviderFamily::Other);
        assert_eq!(family("x", Some("anthropic")), ProviderFamily::Claude);
    }

    #[test]
    fn clipping_scroll_and_wide_characters() {
        let mut c = Canvas::new(10, 3);
        c.clear(0x111111, 0xeeeeee);
        let used = c.clipped(2, 0, 4, 2, 1, |c| {
            c.text(0, 1, "abcdefgh", 0xff0000); // row 1 scrolled to row 0, columns 2..6 only
            c.text(0, 3, "zzzz", 0xff0000) // scrolled to row 2: outside the clip
        });
        assert_eq!(used, 4, "columns are counted even where clipped");
        assert_eq!(c.row_text(0), "  cdef");
        assert_eq!(c.row_text(2), "");
        c.text(0, 1, "日x", 0x00ff00);
        assert_eq!(c.cells[11].ch, CONT);
        c.set(1, 1, 'y', 0, None, false); // over the right half
        assert_eq!(c.row_text(1), " yx");
        assert_eq!(c.right(10, 2, "end", 0), 3);
        assert_eq!(c.row_text(2), "       end");
        assert_eq!(
            c.text_with(0, 2, "日本", 0, None, false, 3),
            2,
            "max stops before a split wide char"
        );
    }

    #[test]
    fn big_digits_and_bars() {
        let mut c = Canvas::new(40, 6);
        c.clear(0, 0xffffff);
        assert_eq!(draw_big(&mut c, 0, 0, "10", 0xffffff, 0x777777, true), 17);
        assert_eq!(big_width("10", true), 17);
        assert_eq!(c.row_text(0), "  ██╗     █████╗");
        assert_eq!(
            c.get(4, 0).unwrap().fg,
            0x777777,
            "shadow strokes take the shade"
        );
        let mut c = Canvas::new(20, 3);
        assert_eq!(draw_big(&mut c, 0, 0, "1%", 1, 2, false), 6);
        assert_eq!(c.row_text(0), " ▄");
        assert_eq!(c.row_text(1), "▀█");
        assert_eq!(c.row_text(2), "▄█▄  %");
        let mut c = Canvas::new(10, 1);
        hbar(&mut c, 0, 0, 4, 0.53, 1, 2);
        assert_eq!(c.row_text(0), "██▏");
    }

    #[test]
    fn rows_coalesce_runs() {
        let mut c = Canvas::new(12, 1);
        c.clear(0x10, 0x20);
        c.text(0, 0, "ab cd", 0xaa);
        c.text_bg(6, 0, "ef", 0xbb, 0x30, true);
        c.set(9, 0, '█', 0xcc, None, false);
        c.set(10, 0, '✓', 0xdd, None, false);
        let p = plan_row(&c, 0, base_bg(&c));
        if paint_of('█') == Paint::Line {
            assert_eq!(p.line, "ab cd ef █  ");
            assert_eq!(
                p.spans,
                vec![
                    (0, 5, 0xaa, false),
                    (6, 8, 0xbb, true),
                    (9, 12, 0xcc, false)
                ]
            );
            assert!(p.vectors.is_empty());
        } else {
            assert_eq!(p.line, "ab cd ef    ");
            assert_eq!(p.spans, vec![(0, 5, 0xaa, false), (6, 8, 0xbb, true)]);
            assert_eq!(p.vectors, vec![9]);
        }
        assert_eq!(p.fills, vec![(6, 8, 0x30)]);
        assert_eq!(p.pinned, vec![(10, 1, '✓', 0xdd, false)]);
    }

    #[test]
    fn every_dashboard_glyph_is_covered_or_drawn() {
        for ch in "±·»×–—‖•…←↑→↓↩⇉−∴≡⊕⊘⌥⏎␣─│┃┄┊└├═║╔╗╚╝╭╮╯╰╵▀▁▂▃▄▅▆▇█▉▊▋▌▍▎▏░▔▕■▲△▸▼◆◇◈◉○◌◎●◔✓✗❯⠇⠋⠏⠙⠦⠧⠴⠸⠹⠼Δ".chars() {
            assert!(paint_of(ch) == Paint::Vector || font_covers(ch), "{ch:?} would fall back");
        }
        for ch in "╔═╗║╚╝─│┌┐└┘├┤┬┴┼━┃╭╮╯╰┄┊▀▄█▌▐▏░⠿".chars()
        {
            assert!(
                matches!(paint_of(ch), Paint::Line | Paint::Vector),
                "{ch:?} must use the fixed-pitch font or vector fallback"
            );
        }
        assert_eq!(paint_of('a'), Paint::Line);
        assert_eq!(paint_of('日'), Paint::Pinned(2));
    }
}

/// Compact latest-run duration (format.ts span).
pub fn span(sec: Option<f64>) -> String {
    let Some(sec) = sec.filter(|s| s.is_finite()) else {
        return "--".into();
    };
    let s = sec.floor().max(0.);
    if s < 60.0 {
        return format!("{}s", js_number(s));
    }
    let (lead, unit, rest) = if s < 3600.0 {
        ((s / 60.0).floor(), "m", format!("{}s", pad2(s % 60.0)))
    } else if s < 86400.0 {
        (
            (s / 3600.0).floor(),
            "h",
            format!("{}m", pad2(((s % 3600.0) / 60.0).floor())),
        )
    } else {
        (
            (s / 86400.0).floor(),
            "d",
            format!("{}h", js_number(((s % 86400.0) / 3600.0).floor())),
        )
    };
    if lead < 10.0 {
        format!("{}{unit}{rest}", js_number(lead))
    } else {
        format!("{}{unit}", js_number(lead))
    }
}
pub fn clock(t: f64, day: f64) -> String {
    fn local(t: f64) -> crate::platform::LocalTime {
        // `new Date(t)` first clips to whole milliseconds toward zero, then the epoch
        // second is the floor of that value (so negative fractions stay on the same second).
        // A failed conversion leaves every field zero, as an untouched `tm` did.
        crate::platform::local_time((t.trunc() / 1000.).floor() as i64).unwrap_or_default()
    }
    let d = local(t);
    let reference = local(day);
    let hm = format!("{:02}:{:02}", d.hour, d.minute);
    if (d.year, d.month0, d.day) == (reference.year, reference.month0, reference.day) {
        hm
    } else {
        format!(
            "{} {} {hm}",
            [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"
            ][d.month0 as usize],
            d.day
        )
    }
}
pub fn when(start: f64, end: Option<f64>, now: f64) -> String {
    end.map(|end| format!("{} → {}", clock(start, now), clock(end, start)))
        .unwrap_or_else(|| format!("since {}", clock(start, now)))
}

#[cfg(test)]
mod timing_tests {
    use super::*;
    #[test]
    fn compact_span_boundaries() {
        for (sec, want) in [
            (0., "0s"),
            (36., "36s"),
            (59.9, "59s"),
            (60., "1m00s"),
            (392., "6m32s"),
            (599., "9m59s"),
            (600., "10m"),
            (1620., "27m"),
            (3600., "1h00m"),
            (17400., "4h50m"),
            (35999., "9h59m"),
            (36000., "10h"),
            (104400., "1d5h"),
            (1036800., "12d"),
            (-5., "0s"),
        ] {
            assert_eq!(span(Some(sec)), want);
        }
        assert_eq!(span(None), "--");
        assert_eq!(span(Some(f64::NAN)), "--");
        let mut sec = 0.;
        while sec < 400. * 86400. {
            assert!(span(Some(sec)).len() <= 5);
            sec = sec * 1.07 + 1.;
        }
    }
    #[test]
    fn javascript_number_formatting_and_to_fixed_ties() {
        // Values printed by Node 26 (String(x) and toFixed), used by tokens/duration/span.
        for (x, want) in [
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (1e-7, "1e-7"),
            (1.5e-7, "1.5e-7"),
            (123456789012345680000., "123456789012345680000"),
            (0.1, "0.1"),
            (-0., "0"),
        ] {
            assert_eq!(js_number(x), want, "{x:?}");
        }
        for (n, want) in [
            (1250., "1.3K"),
            (1_250_000., "1.3M"),
            (100_500_000., "101M"),
            (-7_891_563_968_732_953., "-7891563968732953"),
            (2.5, "3"),
            (1.25, "1"),
            (1e300, "1.0000000000000001e+291B"),
            (305_205_031_763_762_200_000., "305205031763.8B"),
        ] {
            assert_eq!(tokens(n), want, "{n:?}");
        }
        for (sec, want) in [
            (3.9250587206333875e21, "45428920377701250d 0h"),
            (305_205_031_763_762_200_000., "3532465645413914d 13h"),
            (2.2898244182579218e27, "2.650259743354076e+22d 8h"),
            (1e300, "1.1574074074074075e+295d 17h"),
        ] {
            assert_eq!(duration(sec), want, "{sec:?}");
        }
        for (sec, want) in [
            (3.9250587206333875e21, "45428920377701250d"),
            (305_205_031_763_762_200_000., "3532465645413914d"),
            (2.2898244182579218e27, "2.650259743354076e+22d"),
        ] {
            assert_eq!(span(Some(sec)), want, "{sec:?}");
        }
    }

    #[test]
    fn wrapping_uses_javascript_whitespace() {
        // `\s` includes U+FEFF and not U+0085; Rust's split_whitespace is the other way round.
        assert_eq!(wrap("a\u{feff}b", 5), vec!["a b"]);
        assert_eq!(wrap("a\u{feff}b", 2), vec!["a", "b"]);
        assert_eq!(wrap("a\u{85}b", 5), vec!["a\u{85}b"]);
        assert_eq!(wrap("a\u{a0}b", 5), vec!["a b"]);
        assert_eq!(wrap("a\u{c}b", 5), vec!["a b"]);
        assert_eq!(wrap("a\u{8}b", 2), vec!["a\u{8}", "b"]);
        assert_eq!(fit("中中中", 3), "中…");
        assert_eq!(fit("中中中", 2), "…");
    }

    #[test]
    fn locale_compare_matches_node_for_timestamp_ids() {
        use std::cmp::Ordering::{Greater, Less};
        // Signs printed by Node 26 for the same pairs.
        for (a, b, want) in [
            ("a", "B", Less),
            ("a-b", "a_b", Greater),
            ("a", "A", Less),
            ("A", "a", Greater),
            ("aA", "Aa", Less),
            ("a", "ab", Less),
            ("", "a", Less),
            ("2026-11-12T11:00:00Z", "2026-11-12T10:59:59Z", Greater),
            ("thread:task:a", "thread_task:a", Greater),
            ("A1", "a1", Greater),
        ] {
            assert_eq!(locale_compare(a, b), want, "{a:?} vs {b:?}");
        }
        assert_eq!(locale_compare("same", "same"), std::cmp::Ordering::Equal);
    }

    #[test]
    fn wall_clock_dates_use_local_calendar_days() {
        // Construct local times so this test also passes outside the gates' UTC timezone.
        fn local(day: i32, h: i32, m: i32) -> f64 {
            crate::platform::local_to_epoch(2026, 11, i64::from(day), i64::from(h), i64::from(m), 0)
                .unwrap() as f64
                * 1000.
        }
        let now = local(12, 14, 30);
        assert_eq!(clock(local(12, 14, 2), now), "14:02");
        assert_eq!(clock(local(11, 9, 5), now), "Nov 11 09:05");
        assert_eq!(when(local(12, 13, 51), None, now), "since 13:51");
        assert_eq!(
            when(local(11, 23, 50), Some(local(12, 0, 12)), now),
            "Nov 11 23:50 → Nov 12 00:12"
        );
        assert_eq!(
            when(local(10, 8, 0), Some(local(10, 8, 30)), now),
            "Nov 10 08:00 → 08:30"
        );
    }
}
