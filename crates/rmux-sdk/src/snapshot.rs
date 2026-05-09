//! Inert pane snapshot DTOs for SDK consumers.
//!
//! These types model an already-captured pane grid. They do not parse
//! terminal output, resolve tmux targets, or depend on RMUX core/server
//! internals.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A captured pane grid in row-major cell order.
///
/// `revision` is a daemon-derived counter that changes whenever the captured
/// pane state mutates — output, resize, clear, exit, or any other visible
/// change. Consumers use it as the canonical "did the pane move?" signal;
/// there is no separate `current_revision()` getter on the SDK pane handle,
/// because the only authoritative point-in-time revision value is the one
/// carried by a freshly captured snapshot (or by a revision-carrying
/// [`PaneEvent`](crate::PaneEvent) variant).
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct PaneSnapshot {
    /// Visible pane width in terminal columns.
    pub cols: u16,
    /// Visible pane height in terminal rows.
    pub rows: u16,
    /// Row-major cells, with `row * cols + col` indexing.
    pub cells: Vec<PaneCell>,
    /// Captured cursor coordinates and state.
    pub cursor: PaneCursor,
    /// Daemon-derived revision counter for this captured pane state.
    ///
    /// The producer guarantees that when any observable pane field
    /// (`cols`, `rows`, `cells`, `cursor`, the underlying process state)
    /// changes between two captures, the revision changes too. Equal
    /// revisions therefore mean "nothing observable changed". A captured
    /// snapshot for an exited or no-longer-listed pane carries a revision
    /// distinct from any prior live revision.
    pub revision: u64,
}

impl PaneSnapshot {
    /// Creates a snapshot after checking the row-major cell count.
    ///
    /// The expected cell count is `rows * cols`. Zero-sized dimensions are
    /// allowed and therefore expect zero cells. The revision defaults to `0`;
    /// use [`Self::with_revision`] to attach a daemon-derived revision.
    pub fn new(
        cols: u16,
        rows: u16,
        cells: Vec<PaneCell>,
        cursor: PaneCursor,
    ) -> Result<Self, PaneSnapshotShapeError> {
        let snapshot = Self {
            cols,
            rows,
            cells,
            cursor,
            revision: 0,
        };
        snapshot.validate_shape()?;
        Ok(snapshot)
    }

    /// Returns a copy of this snapshot with the supplied revision.
    #[must_use]
    pub fn with_revision(mut self, revision: u64) -> Self {
        self.revision = revision;
        self
    }

    /// Returns the number of row-major cells implied by `rows * cols`.
    #[must_use]
    pub fn expected_cell_count(&self) -> usize {
        expected_cell_count(self.cols, self.rows)
    }

    /// Returns whether `cells.len()` exactly matches `rows * cols`.
    #[must_use]
    pub fn is_row_major_shape(&self) -> bool {
        self.cells.len() == self.expected_cell_count()
    }

    /// Checks the row-major cell-count invariant.
    pub fn validate_shape(&self) -> Result<(), PaneSnapshotShapeError> {
        let expected = self.expected_cell_count();
        if self.cells.len() == expected {
            Ok(())
        } else {
            Err(PaneSnapshotShapeError {
                cols: self.cols,
                rows: self.rows,
                actual_cells: self.cells.len(),
                expected_cells: expected,
            })
        }
    }

    /// Returns one cell by visible row and column.
    #[must_use]
    pub fn cell(&self, row: u16, col: u16) -> Option<&PaneCell> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        let index = usize::from(row)
            .saturating_mul(usize::from(self.cols))
            .saturating_add(usize::from(col));
        self.cells.get(index)
    }

    /// Returns one row slice by visible row.
    ///
    /// Malformed snapshots with too few cells return `None` for incomplete
    /// rows rather than panicking.
    #[must_use]
    pub fn row_cells(&self, row: u16) -> Option<&[PaneCell]> {
        if row >= self.rows {
            return None;
        }

        let cols = usize::from(self.cols);
        let start = usize::from(row).checked_mul(cols)?;
        let end = start.checked_add(cols)?;
        self.cells.get(start..end)
    }

    /// Resolves the owning non-padding cell column for a visible position.
    ///
    /// If the addressed cell is not padding, its own column is returned. If it
    /// is padding for a wide glyph, the leading glyph column is returned only
    /// when that glyph's recorded display width spans the requested column.
    #[must_use]
    pub fn owning_cell_col(&self, row: u16, col: u16) -> Option<u16> {
        let cell = self.cell(row, col)?;
        if !cell.is_padding() {
            return Some(col);
        }

        let mut owner = col;
        while owner > 0 {
            owner -= 1;
            let candidate = self.cell(row, owner)?;
            if !candidate.is_padding() {
                let width = u16::from(candidate.glyph.width.max(1));
                if owner.saturating_add(width) > col {
                    return Some(owner);
                }
                return None;
            }
        }

        None
    }

    /// Iterates visible, non-padding cells with their original row and column.
    ///
    /// Padding cells belonging to wide glyphs are skipped, while the leading
    /// glyph keeps its original display column.
    pub fn visible_cells(&self) -> impl Iterator<Item = (u16, u16, &PaneCell)> + '_ {
        let cols = usize::from(self.cols);
        let rows = usize::from(self.rows);
        self.cells
            .iter()
            .enumerate()
            .filter_map(move |(index, cell)| {
                if cols == 0 || cell.is_padding() {
                    return None;
                }

                let row = index / cols;
                if row >= rows {
                    return None;
                }
                let col = index % cols;
                Some((row as u16, col as u16, cell))
            })
    }

    /// Renders one visible row using RMUX core's lossy plain-text behavior.
    ///
    /// Padding cells are skipped and trailing space characters are trimmed.
    /// Other whitespace and control-like payloads are preserved verbatim. If a
    /// malformed snapshot ends partway through this row, the available cells
    /// are rendered instead of panicking.
    #[must_use]
    pub fn visible_row_text(&self, row: u16) -> Option<String> {
        self.lossy_row_cells(row).map(render_cells_lossy)
    }

    /// Renders one visible row, returning an empty string for out-of-bounds rows.
    #[must_use]
    pub fn row_text(&self, row: u16) -> String {
        self.visible_row_text(row).unwrap_or_default()
    }

    /// Renders all visible rows using lossy plain-text behavior.
    ///
    /// Incomplete malformed rows render their available cells instead of
    /// panicking.
    #[must_use]
    pub fn visible_lines(&self) -> Vec<String> {
        (0..self.rows)
            .map(|row| self.visible_row_text(row).unwrap_or_default())
            .collect()
    }

    /// Renders all visible rows joined by `\n`.
    ///
    /// The returned string has no synthetic trailing newline.
    #[must_use]
    pub fn visible_text(&self) -> String {
        self.visible_lines().join("\n")
    }

    fn lossy_row_cells(&self, row: u16) -> Option<&[PaneCell]> {
        if row >= self.rows {
            return None;
        }

        let cols = usize::from(self.cols);
        if cols == 0 {
            return Some(&[]);
        }

        let start = usize::from(row).checked_mul(cols)?;
        if start >= self.cells.len() {
            return Some(&[]);
        }
        let end = start.saturating_add(cols).min(self.cells.len());
        Some(&self.cells[start..end])
    }
}

impl Serialize for PaneSnapshot {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate_shape().map_err(serde::ser::Error::custom)?;
        PaneSnapshotFieldsRef {
            cols: self.cols,
            rows: self.rows,
            cells: &self.cells,
            cursor: &self.cursor,
            revision: self.revision,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PaneSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let fields = PaneSnapshotFields::deserialize(deserializer)?;
        Self::new(fields.cols, fields.rows, fields.cells, fields.cursor)
            .map(|snapshot| snapshot.with_revision(fields.revision))
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize)]
struct PaneSnapshotFieldsRef<'a> {
    cols: u16,
    rows: u16,
    cells: &'a [PaneCell],
    cursor: &'a PaneCursor,
    revision: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PaneSnapshotFields {
    cols: u16,
    rows: u16,
    cells: Vec<PaneCell>,
    cursor: PaneCursor,
    revision: u64,
}

/// Error returned when a snapshot's dimensions do not match its cell vector.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PaneSnapshotShapeError {
    cols: u16,
    rows: u16,
    actual_cells: usize,
    expected_cells: usize,
}

impl PaneSnapshotShapeError {
    /// Returns the snapshot column count.
    #[must_use]
    pub const fn cols(&self) -> u16 {
        self.cols
    }

    /// Returns the snapshot row count.
    #[must_use]
    pub const fn rows(&self) -> u16 {
        self.rows
    }

    /// Returns the actual number of cells supplied.
    #[must_use]
    pub const fn actual_cells(&self) -> usize {
        self.actual_cells
    }

    /// Returns the expected `rows * cols` cell count.
    #[must_use]
    pub const fn expected_cells(&self) -> usize {
        self.expected_cells
    }
}

impl fmt::Display for PaneSnapshotShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pane snapshot shape mismatch: {}x{} expects {} cells, got {}",
            self.cols, self.rows, self.expected_cells, self.actual_cells
        )
    }
}

impl std::error::Error for PaneSnapshotShapeError {}

/// One captured pane cell.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneCell {
    /// Captured glyph payload and display-width metadata.
    #[serde(default)]
    pub glyph: PaneGlyph,
    /// Cell attribute bitset.
    #[serde(default)]
    pub attributes: PaneAttributes,
    /// Foreground color.
    #[serde(default)]
    pub foreground: PaneColor,
    /// Background color.
    #[serde(default)]
    pub background: PaneColor,
    /// Underline color.
    #[serde(default)]
    pub underline: PaneColor,
}

impl PaneCell {
    /// Creates a cell with the given glyph and default style.
    #[must_use]
    pub fn new(glyph: PaneGlyph) -> Self {
        Self {
            glyph,
            ..Self::default()
        }
    }

    /// Creates a blank, non-padding cell with default style.
    #[must_use]
    pub fn blank() -> Self {
        Self::new(PaneGlyph::blank())
    }

    /// Creates a padding cell for the trailing column of a wide glyph.
    #[must_use]
    pub fn padding() -> Self {
        Self::new(PaneGlyph::padding())
    }

    /// Returns whether this cell is wide-glyph padding.
    #[must_use]
    pub const fn is_padding(&self) -> bool {
        self.glyph.is_padding()
    }

    /// Returns the stored glyph text payload.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.glyph.text
    }
}

impl Default for PaneCell {
    fn default() -> Self {
        Self {
            glyph: PaneGlyph::blank(),
            attributes: PaneAttributes::EMPTY,
            foreground: PaneColor::Default,
            background: PaneColor::Default,
            underline: PaneColor::Default,
        }
    }
}

/// Captured glyph payload for one grid cell.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneGlyph {
    /// Stored text payload.
    #[serde(default = "default_glyph_text")]
    pub text: String,
    /// Display width recorded by the terminal grid.
    #[serde(default = "default_glyph_width")]
    pub width: u8,
    /// Whether this is padding for a preceding wide glyph.
    #[serde(default)]
    pub padding: bool,
}

impl PaneGlyph {
    /// Creates a non-padding glyph from already-recorded text and width.
    #[must_use]
    pub fn new(text: impl Into<String>, width: u8) -> Self {
        Self {
            text: text.into(),
            width,
            padding: false,
        }
    }

    /// Creates a blank, single-width glyph.
    #[must_use]
    pub fn blank() -> Self {
        Self {
            text: " ".to_owned(),
            width: 1,
            padding: false,
        }
    }

    /// Creates a padding marker for the trailing column of a wide glyph.
    #[must_use]
    pub fn padding() -> Self {
        Self {
            text: " ".to_owned(),
            width: 0,
            padding: true,
        }
    }

    /// Returns whether this glyph is a padding marker.
    #[must_use]
    pub const fn is_padding(&self) -> bool {
        self.padding
    }
}

impl Default for PaneGlyph {
    fn default() -> Self {
        Self::blank()
    }
}

/// Color encoding carried by a captured pane cell.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PaneColor {
    /// Terminal default color sentinel.
    #[default]
    Default,
    /// Explicit no-color sentinel.
    None,
    /// Terminal color sentinel.
    Terminal,
    /// Standard ANSI color value `0..=7`.
    Ansi {
        /// Standard ANSI palette index.
        index: u8,
    },
    /// Bright ANSI color value `90..=97`.
    BrightAnsi {
        /// Bright ANSI palette index.
        index: u8,
    },
    /// 256-color palette value encoded with the RMUX/tmux 256-color flag.
    Indexed {
        /// 256-color palette index.
        index: u8,
    },
    /// True-color RGB value encoded with the RMUX/tmux RGB flag.
    Rgb {
        /// Red component.
        red: u8,
        /// Green component.
        green: u8,
        /// Blue component.
        blue: u8,
    },
    /// Unknown or future raw color encoding.
    Encoded {
        /// Raw encoded color value.
        value: i32,
    },
}

impl PaneColor {
    /// Raw encoding for the terminal default color.
    pub const DEFAULT_ENCODING: i32 = 8;
    /// Raw encoding for the explicit no-color sentinel.
    pub const NONE_ENCODING: i32 = -1;
    /// Raw encoding for the terminal color sentinel.
    pub const TERMINAL_ENCODING: i32 = 9;
    /// Raw flag for 256-color palette values.
    pub const INDEXED_FLAG: i32 = 0x0100_0000;
    /// Raw flag for true-color RGB values.
    pub const RGB_FLAG: i32 = 0x0200_0000;

    /// Creates a standard ANSI color value.
    #[must_use]
    pub const fn ansi(index: u8) -> Self {
        Self::Ansi { index }
    }

    /// Creates a bright ANSI color value.
    #[must_use]
    pub const fn bright_ansi(index: u8) -> Self {
        Self::BrightAnsi { index }
    }

    /// Creates a 256-color palette value.
    #[must_use]
    pub const fn indexed(index: u8) -> Self {
        Self::Indexed { index }
    }

    /// Creates an RGB true-color value.
    #[must_use]
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self::Rgb { red, green, blue }
    }

    /// Creates a color DTO from a raw RMUX/tmux-compatible encoding.
    #[must_use]
    pub fn from_encoded(value: i32) -> Self {
        match value {
            Self::NONE_ENCODING => Self::None,
            Self::DEFAULT_ENCODING => Self::Default,
            Self::TERMINAL_ENCODING => Self::Terminal,
            0..=7 => Self::Ansi { index: value as u8 },
            90..=97 => Self::BrightAnsi {
                index: (value - 90) as u8,
            },
            _ if value & !(Self::INDEXED_FLAG | 0xff) == 0 && value & Self::INDEXED_FLAG != 0 => {
                Self::Indexed {
                    index: (value & 0xff) as u8,
                }
            }
            _ if value & !(Self::RGB_FLAG | 0x00ff_ffff) == 0 && value & Self::RGB_FLAG != 0 => {
                Self::Rgb {
                    red: ((value >> 16) & 0xff) as u8,
                    green: ((value >> 8) & 0xff) as u8,
                    blue: (value & 0xff) as u8,
                }
            }
            _ => Self::Encoded { value },
        }
    }

    /// Returns the raw RMUX/tmux-compatible color encoding.
    #[must_use]
    pub const fn encoded(self) -> i32 {
        match self {
            Self::Default => Self::DEFAULT_ENCODING,
            Self::None => Self::NONE_ENCODING,
            Self::Terminal => Self::TERMINAL_ENCODING,
            Self::Ansi { index } => index as i32,
            Self::BrightAnsi { index } => 90 + index as i32,
            Self::Indexed { index } => Self::INDEXED_FLAG | index as i32,
            Self::Rgb { red, green, blue } => {
                Self::RGB_FLAG | ((red as i32) << 16) | ((green as i32) << 8) | blue as i32
            }
            Self::Encoded { value } => value,
        }
    }
}

/// Cell attribute bits matching the RMUX/tmux grid attribute bit layout.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneAttributes {
    /// Raw attribute bitset.
    pub bits: u16,
}

impl PaneAttributes {
    /// Empty attribute bitset.
    pub const EMPTY: Self = Self { bits: 0 };
    /// Bold attribute bit.
    pub const BOLD: Self = Self { bits: 0x1 };
    /// tmux-compatible alias for [`Self::BOLD`].
    pub const BRIGHT: Self = Self::BOLD;
    /// Dim attribute bit.
    pub const DIM: Self = Self { bits: 0x2 };
    /// Single underline attribute bit.
    pub const UNDERLINE: Self = Self { bits: 0x4 };
    /// tmux-compatible alias for [`Self::UNDERLINE`].
    pub const UNDERSCORE: Self = Self::UNDERLINE;
    /// Blink attribute bit.
    pub const BLINK: Self = Self { bits: 0x8 };
    /// Reverse-video attribute bit.
    pub const REVERSE: Self = Self { bits: 0x10 };
    /// Hidden attribute bit.
    pub const HIDDEN: Self = Self { bits: 0x20 };
    /// Italic attribute bit.
    pub const ITALIC: Self = Self { bits: 0x40 };
    /// tmux-compatible alias for [`Self::ITALIC`].
    pub const ITALICS: Self = Self::ITALIC;
    /// ACS line-drawing charset attribute bit.
    pub const CHARSET: Self = Self { bits: 0x80 };
    /// Strikethrough attribute bit.
    pub const STRIKETHROUGH: Self = Self { bits: 0x100 };
    /// Double underline attribute bit.
    pub const DOUBLE_UNDERLINE: Self = Self { bits: 0x200 };
    /// Curly underline attribute bit.
    pub const CURLY_UNDERLINE: Self = Self { bits: 0x400 };
    /// Dotted underline attribute bit.
    pub const DOTTED_UNDERLINE: Self = Self { bits: 0x800 };
    /// Dashed underline attribute bit.
    pub const DASHED_UNDERLINE: Self = Self { bits: 0x1000 };
    /// Overline attribute bit.
    pub const OVERLINE: Self = Self { bits: 0x2000 };
    /// Explicit no-inherited-attributes bit.
    pub const NO_ATTRIBUTES: Self = Self { bits: 0x4000 };
    /// tmux-compatible alias for [`Self::NO_ATTRIBUTES`].
    pub const NOATTR: Self = Self::NO_ATTRIBUTES;
    /// All underline variant bits combined.
    pub const ALL_UNDERSCORE: Self = Self {
        bits: Self::UNDERLINE.bits
            | Self::DOUBLE_UNDERLINE.bits
            | Self::CURLY_UNDERLINE.bits
            | Self::DOTTED_UNDERLINE.bits
            | Self::DASHED_UNDERLINE.bits,
    };

    /// Creates an attribute set from raw bits.
    #[must_use]
    pub const fn from_bits(bits: u16) -> Self {
        Self { bits }
    }

    /// Returns the raw attribute bits.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.bits
    }

    /// Returns whether this bitset contains every bit in `other`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.bits & other.bits == other.bits
    }

    /// Returns whether no attribute bits are set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }
}

impl std::ops::BitOr for PaneAttributes {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self {
            bits: self.bits | rhs.bits,
        }
    }
}

impl std::ops::BitOrAssign for PaneAttributes {
    fn bitor_assign(&mut self, rhs: Self) {
        self.bits |= rhs.bits;
    }
}

impl std::ops::BitAnd for PaneAttributes {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self {
            bits: self.bits & rhs.bits,
        }
    }
}

/// Captured cursor coordinates and rendering state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneCursor {
    /// Zero-based cursor row within the visible pane.
    #[serde(default)]
    pub row: u16,
    /// Zero-based cursor column within the visible pane.
    #[serde(default)]
    pub col: u16,
    /// Whether the cursor is visible.
    #[serde(default = "default_cursor_visible")]
    pub visible: bool,
    /// Raw cursor style value.
    #[serde(default)]
    pub style: u32,
}

impl PaneCursor {
    /// Creates a cursor DTO from plain coordinates and state.
    #[must_use]
    pub const fn new(row: u16, col: u16, visible: bool, style: u32) -> Self {
        Self {
            row,
            col,
            visible,
            style,
        }
    }
}

impl Default for PaneCursor {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            visible: true,
            style: 0,
        }
    }
}

fn expected_cell_count(cols: u16, rows: u16) -> usize {
    usize::from(cols) * usize::from(rows)
}

fn default_glyph_text() -> String {
    " ".to_owned()
}

const fn default_glyph_width() -> u8 {
    1
}

const fn default_cursor_visible() -> bool {
    true
}

fn render_cells_lossy(cells: &[PaneCell]) -> String {
    let mut rendered = String::new();
    for cell in cells {
        if cell.is_padding() {
            continue;
        }
        rendered.push_str(cell.text());
    }
    while rendered.ends_with(' ') {
        rendered.pop();
    }
    rendered
}
