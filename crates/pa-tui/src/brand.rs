//! The brand mark: a red spider lily (Lycoris radiata) in quadrant blocks,
//! 7 rows by 21 columns, shared by the chat splash, the first-run screen, and
//! (ported cell for cell) the installer banner in `install-rust.sh`.

use crate::theme::ThemeColor;

/// The mark's rows. Each cell is a 2x2 quadrant glyph.
pub const LOGO_ROWS: [&str; 7] = [
    "▗  ▐▄  ▙     ▗▌ ▗▟  ▗",
    "▝▜▄ ▝▚▄▝▜▄ ▗▟▀▗▄▀ ▗▟▀",
    "  ▝▀▙▄▟▀██▙██▛▜▄▄▛▀",
    "      ▝█▀▜██▀▜▛",
    "       ▝▚▛▐▝▙▀",
    "          ▐",
    "          ▐",
];

/// The tone of each cell in [`LOGO_ROWS`]: `p` petals and stamens, `a`
/// anthers, `s` the stem, a space for an empty cell.
const LOGO_TONES: [&str; 7] = [
    "a  ap  a     pa pa  a",
    "ppp pppppp pppppp ppp",
    "  ppppppppppppppppp",
    "      ppppppppp",
    "       pppsppp",
    "          s",
    "          s",
];

/// The widest row, in columns.
pub const LOGO_WIDTH: usize = 21;

/// The theme color of the mark's cell at `row`, `column`.
#[must_use]
pub fn logo_tone(row: usize, column: usize) -> ThemeColor {
    match LOGO_TONES
        .get(row)
        .and_then(|tones| tones.chars().nth(column))
    {
        Some('a') => ThemeColor::Warning,
        Some('s') => ThemeColor::Success,
        _ => ThemeColor::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_glyph_has_a_tone_and_every_tone_a_glyph() {
        for (row, (glyphs, tones)) in LOGO_ROWS.iter().zip(LOGO_TONES).enumerate() {
            assert_eq!(glyphs.chars().count(), tones.chars().count(), "row {row}");
            for (column, (glyph, tone)) in glyphs.chars().zip(tones.chars()).enumerate() {
                assert_eq!(glyph == ' ', tone == ' ', "row {row} column {column}");
            }
        }
        let widest = LOGO_ROWS.iter().map(|row| row.chars().count()).max();
        assert_eq!(widest, Some(LOGO_WIDTH));
    }
}
