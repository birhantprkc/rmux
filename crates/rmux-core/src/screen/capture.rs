use crate::grid::{Grid, GridCapture, GridRenderOptions, GridStringState};
use crate::hyperlinks::Hyperlinks;
use crate::transcript::{resolve_screen_capture_range, ScreenCaptureRange};

use super::Screen;

impl Screen {
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub(crate) fn capture_grid(&self, join_wrapped: bool) -> GridCapture {
        self.grid.capture(join_wrapped)
    }

    /// Captures a tmux-style line range over the current grid contents.
    #[must_use]
    pub fn capture_transcript(
        &self,
        range: ScreenCaptureRange,
        options: GridRenderOptions,
    ) -> Vec<u8> {
        capture_grid_bytes(&self.grid, &self.hyperlinks, range, options)
    }

    /// Captures the saved pre-alternate-screen copy when alternate mode is active.
    #[must_use]
    pub fn capture_saved_transcript(
        &self,
        range: ScreenCaptureRange,
        options: GridRenderOptions,
    ) -> Option<Vec<u8>> {
        self.saved_grid
            .as_ref()
            .map(|saved| capture_grid_bytes(&saved.grid, &self.hyperlinks, range, options))
    }
}

fn capture_grid_bytes(
    grid: &Grid,
    hyperlinks: &Hyperlinks,
    range: ScreenCaptureRange,
    options: GridRenderOptions,
) -> Vec<u8> {
    let total_lines = grid.hsize() + usize::try_from(grid.sy()).unwrap_or(usize::MAX);
    let Some(range) = resolve_screen_capture_range(range, grid.hsize(), total_lines) else {
        return Vec::new();
    };

    let mut output = Vec::new();
    let mut state = GridStringState::default();
    for absolute_y in range {
        let Some(line) =
            grid.render_absolute_line(absolute_y, options, &mut state, Some(hyperlinks))
        else {
            continue;
        };
        output.extend_from_slice(line.as_bytes());
        let wrapped = grid.absolute_line_wrapped(absolute_y).unwrap_or(false);
        if !options.join_wrapped || !wrapped {
            output.push(b'\n');
        }
    }
    output
}
