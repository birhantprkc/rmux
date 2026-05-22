/// Opaque terminal command that must be forwarded to a capable outer terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalPassthrough {
    kind: TerminalPassthroughKind,
    cursor_x: u32,
    cursor_y: u32,
    payload: Vec<u8>,
}

/// Supported terminal passthrough protocol families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalPassthroughKind {
    /// Kitty terminal graphics protocol, encoded as an APC payload.
    KittyGraphics,
}

impl TerminalPassthrough {
    /// Creates a Kitty graphics passthrough event at a pane-local cursor position.
    #[must_use]
    pub fn kitty_graphics(cursor_x: u32, cursor_y: u32, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            kind: TerminalPassthroughKind::KittyGraphics,
            cursor_x,
            cursor_y,
            payload: payload.into(),
        }
    }

    /// Returns the passthrough protocol family.
    #[must_use]
    pub const fn kind(&self) -> TerminalPassthroughKind {
        self.kind
    }

    /// Returns the pane-local cursor column captured when the sequence arrived.
    #[must_use]
    pub const fn cursor_x(&self) -> u32 {
        self.cursor_x
    }

    /// Returns the pane-local cursor row captured when the sequence arrived.
    #[must_use]
    pub const fn cursor_y(&self) -> u32 {
        self.cursor_y
    }

    /// Returns the opaque protocol payload without escape framing.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Renders the passthrough as an outer-terminal escape sequence.
    #[must_use]
    pub fn render_sequence(&self) -> Vec<u8> {
        match self.kind {
            TerminalPassthroughKind::KittyGraphics => {
                let mut sequence = Vec::with_capacity(self.payload.len() + 4);
                sequence.extend_from_slice(b"\x1b_");
                sequence.extend_from_slice(&self.payload);
                sequence.extend_from_slice(b"\x1b\\");
                sequence
            }
        }
    }

    /// Returns whether the passthrough payload already carries placement
    /// coordinates that the outer terminal should interpret directly.
    #[must_use]
    pub fn has_explicit_placement(&self) -> bool {
        match self.kind {
            TerminalPassthroughKind::KittyGraphics => {
                kitty_graphics_payload_has_explicit_placement(&self.payload)
            }
        }
    }
}

fn kitty_graphics_payload_has_explicit_placement(payload: &[u8]) -> bool {
    let header = payload
        .split(|byte| *byte == b';')
        .next()
        .unwrap_or(payload);
    header.split(|byte| *byte == b',').any(|field| {
        let field = field.strip_prefix(b"G").unwrap_or(field);
        field.starts_with(b"r=") || field.starts_with(b"c=")
    })
}

#[cfg(test)]
mod tests {
    use super::TerminalPassthrough;

    #[test]
    fn kitty_graphics_detects_explicit_row_or_column_placement() {
        assert!(
            TerminalPassthrough::kitty_graphics(0, 0, b"Ga=p,r=10,c=20;AAAA")
                .has_explicit_placement()
        );
        assert!(TerminalPassthrough::kitty_graphics(0, 0, b"Gr=10;AAAA").has_explicit_placement());
        assert!(!TerminalPassthrough::kitty_graphics(0, 0, b"Gf=100;AAAA").has_explicit_placement());
    }
}
