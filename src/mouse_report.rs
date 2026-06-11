//! Typed mouse-report emission — ONE builder for every xterm mouse
//! protocol byte sequence mado sends to a PTY.
//!
//! Both event loops (local-PTY `main.rs`, tear-attach
//! `gui_tear_attach.rs`) construct a [`MouseReport`] and serialize it
//! here, so the two paths cannot drift and — per ★★ TYPED EMISSION —
//! no escape bytes are ever `format!()`-composed: digits are emitted
//! byte-wise.
//!
//! Encoding reference (xterm ctlseqs):
//! * SGR-1006: `ESC [ < Cb ; Px ; Py M` for press/motion, final `m`
//!   for release (button code preserved on release).
//! * Legacy X10: `ESC [ M Cb Cx Cy` with everything offset by 32 and
//!   coordinates clamped to 223; release is always button code 3.
//! * Motion adds 32 to the button code; "no button held" hover motion
//!   is button code 3 (so SGR hover motion is `35`, NOT a fabricated
//!   left-drag `32` — apps distinguish drag from hover by this bit).

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseReportKind {
    /// Button press (wheel ticks are presses; they have no release).
    Press,
    /// Button release.
    Release,
    /// Pointer motion (with or without a held button).
    Motion,
}

/// Which button, xterm numbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseReportButton {
    Left,
    Middle,
    Right,
    /// No button held — AnyEvent (1003) hover motion.
    None,
    WheelUp,
    WheelDown,
}

impl MouseReportButton {
    fn code(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
            Self::None => 3,
            Self::WheelUp => 64,
            Self::WheelDown => 65,
        }
    }
}

/// One mouse report: kind + button + 1-based cell coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseReport {
    pub kind: MouseReportKind,
    pub button: MouseReportButton,
    /// 1-based column.
    pub col: usize,
    /// 1-based row.
    pub row: usize,
}

impl MouseReport {
    /// Serialize for the negotiated encoding: SGR-1006 when `sgr` is
    /// set (mode 1006), legacy X10 otherwise.
    #[must_use]
    pub fn encode(&self, sgr: bool) -> Vec<u8> {
        let motion = matches!(self.kind, MouseReportKind::Motion);
        let code = self.button.code() + if motion { 32 } else { 0 };
        let mut out = Vec::with_capacity(16);
        if sgr {
            out.extend_from_slice(b"\x1b[<");
            push_decimal(&mut out, code as usize);
            out.push(b';');
            push_decimal(&mut out, self.col);
            out.push(b';');
            push_decimal(&mut out, self.row);
            out.push(match self.kind {
                MouseReportKind::Release => b'm',
                _ => b'M',
            });
        } else {
            let cb = match self.kind {
                // X10 cannot express WHICH button released.
                MouseReportKind::Release => 32 + 3,
                _ => 32 + code,
            };
            let cx = (self.col.min(223)) as u8 + 32;
            let cy = (self.row.min(223)) as u8 + 32;
            out.extend_from_slice(&[0x1b, b'[', b'M', cb, cx, cy]);
        }
        out
    }
}

/// Append `n` as ASCII decimal digits — typed byte-wise emission.
fn push_decimal(buf: &mut Vec<u8>, n: usize) {
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    let mut n = n;
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    buf.extend_from_slice(&digits[i..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(kind: MouseReportKind, button: MouseReportButton, col: usize, row: usize, sgr: bool) -> Vec<u8> {
        MouseReport { kind, button, col, row }.encode(sgr)
    }

    #[test]
    fn sgr_left_press_and_release() {
        assert_eq!(
            enc(MouseReportKind::Press, MouseReportButton::Left, 42, 7, true),
            b"\x1b[<0;42;7M"
        );
        // SGR keeps the button code on release; only the final flips.
        assert_eq!(
            enc(MouseReportKind::Release, MouseReportButton::Left, 42, 7, true),
            b"\x1b[<0;42;7m"
        );
    }

    #[test]
    fn sgr_drag_motion_vs_hover_motion() {
        // Left-drag motion: 0 + 32 = 32.
        assert_eq!(
            enc(MouseReportKind::Motion, MouseReportButton::Left, 3, 4, true),
            b"\x1b[<32;3;4M"
        );
        // Hover motion (no button): 3 + 32 = 35 — apps distinguish
        // drag from hover by exactly this code.
        assert_eq!(
            enc(MouseReportKind::Motion, MouseReportButton::None, 3, 4, true),
            b"\x1b[<35;3;4M"
        );
    }

    #[test]
    fn sgr_wheel() {
        assert_eq!(
            enc(MouseReportKind::Press, MouseReportButton::WheelUp, 1, 1, true),
            b"\x1b[<64;1;1M"
        );
        assert_eq!(
            enc(MouseReportKind::Press, MouseReportButton::WheelDown, 1, 1, true),
            b"\x1b[<65;1;1M"
        );
    }

    #[test]
    fn x10_press_release_and_clamp() {
        // Press left at (1,1): Cb = 32+0, coords 1+32.
        assert_eq!(
            enc(MouseReportKind::Press, MouseReportButton::Left, 1, 1, false),
            &[0x1b, b'[', b'M', 32, 33, 33]
        );
        // Release is always code 3 in X10.
        assert_eq!(
            enc(MouseReportKind::Release, MouseReportButton::Left, 1, 1, false),
            &[0x1b, b'[', b'M', 35, 33, 33]
        );
        // Coordinates clamp at 223 (255 - 32).
        assert_eq!(
            enc(MouseReportKind::Press, MouseReportButton::Left, 500, 500, false),
            &[0x1b, b'[', b'M', 32, 255, 255]
        );
    }

    #[test]
    fn x10_wheel() {
        assert_eq!(
            enc(MouseReportKind::Press, MouseReportButton::WheelUp, 1, 1, false),
            &[0x1b, b'[', b'M', 96, 33, 33]
        );
    }
}
