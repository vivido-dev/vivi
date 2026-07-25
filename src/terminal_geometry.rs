use std::io::{self, Write};
use std::time::{Duration, Instant};

const DEFAULT_CELL_WIDTH_PX: u32 = 10;
const DEFAULT_CELL_HEIGHT_PX: u32 = 20;
const DISPLAY_SETTLE_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(unix)]
const CSI_CELL_SIZE_TIMEOUT: Duration = Duration::from_millis(50);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct TerminalGeometry {
    pub cols: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
}

impl TerminalGeometry {
    pub fn current() -> Self {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        Self::current_with_reported_cells(cols, rows)
    }

    fn current_with_reported_cells(cols: u16, rows: u16) -> Self {
        let mut cols = cols.max(1);
        let mut rows = rows.max(1);

        if let Some(winsize) = ioctl_winsize() {
            cols = winsize.cols.max(1);
            rows = winsize.rows.max(1);
            if let Some((cell_width_px, cell_height_px)) = winsize.cell_size() {
                return Self::with_cell_size(cols, rows, cell_width_px, cell_height_px);
            }
        }

        if let Some((cell_width_px, cell_height_px)) = query_cell_size_with_csi() {
            return Self::with_cell_size(cols, rows, cell_width_px, cell_height_px);
        }

        Self::from_cells(cols, rows)
    }

    pub fn from_cells(cols: u16, rows: u16) -> Self {
        Self::with_cell_size(cols, rows, DEFAULT_CELL_WIDTH_PX, DEFAULT_CELL_HEIGHT_PX)
    }

    fn from_settled_display(display: vivid_sdk::DisplayState) -> Option<Self> {
        if !display.settled {
            return None;
        }
        let cols = u16::try_from(display.grid_columns).ok()?;
        let rows = u16::try_from(display.grid_rows).ok()?;
        (cols > 0 && rows > 0)
            .then(|| Self::with_cell_size(cols, rows, display.cell_width, display.cell_height))
    }

    pub fn settled_presenter(session: &vivid_sdk::ProducerSession) -> Self {
        let deadline = Instant::now() + DISPLAY_SETTLE_TIMEOUT;
        loop {
            if let Some(geometry) = Self::from_settled_display(session.display_state()) {
                return geometry;
            }
            if Instant::now() >= deadline {
                return Self::current();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn with_cell_size(cols: u16, rows: u16, cell_width_px: u32, cell_height_px: u32) -> Self {
        Self {
            cols: cols.max(1),
            rows: rows.max(1),
            cell_width_px: cell_width_px.max(1),
            cell_height_px: cell_height_px.max(1),
        }
    }

    pub fn drawable_width_px(self, margin_cols: u16) -> u32 {
        u32::from(self.cols.saturating_sub(margin_cols).max(1)) * self.cell_width_px
    }

    pub fn drawable_height_px(self, margin_rows: u16) -> u32 {
        u32::from(self.rows.saturating_sub(margin_rows).max(1)) * self.cell_height_px
    }
}

pub fn cells_for_pixels(pixels: u32, cell_pixels: u32) -> u32 {
    let cell_pixels = cell_pixels.max(1);
    pixels.max(1).saturating_add(cell_pixels - 1) / cell_pixels
}

/// Advance the text cursor past an inline media rectangle without putting any media bytes in the
/// PTY. The media itself remains attached to the zero-width anchor at the original cursor.
pub fn reserve_rows(rows: u32) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    for _ in 0..rows {
        stdout.write_all(b"\r\n")?;
    }
    stdout.flush()
}

#[derive(Debug, Copy, Clone)]
struct RawWinsize {
    cols: u16,
    rows: u16,
    xpixel: u32,
    ypixel: u32,
}

impl RawWinsize {
    fn cell_size(self) -> Option<(u32, u32)> {
        let cols = u32::from(self.cols);
        let rows = u32::from(self.rows);
        if cols == 0 || rows == 0 || self.xpixel < 2 * cols || self.ypixel < 4 * rows {
            return None;
        }

        Some(((self.xpixel / cols).max(1), (self.ypixel / rows).max(1)))
    }
}

#[cfg(unix)]
fn ioctl_winsize() -> Option<RawWinsize> {
    for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO, libc::STDIN_FILENO] {
        if unsafe { libc::isatty(fd) } != 1 {
            continue;
        }

        let mut winsize = std::mem::MaybeUninit::<libc::winsize>::zeroed();
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, winsize.as_mut_ptr()) } != 0 {
            continue;
        }
        let winsize = unsafe { winsize.assume_init() };
        if winsize.ws_col > 0 && winsize.ws_row > 0 {
            return Some(RawWinsize {
                cols: winsize.ws_col,
                rows: winsize.ws_row,
                xpixel: u32::from(winsize.ws_xpixel),
                ypixel: u32::from(winsize.ws_ypixel),
            });
        }
    }

    None
}

#[cfg(not(unix))]
fn ioctl_winsize() -> Option<RawWinsize> {
    None
}

#[cfg(unix)]
fn query_cell_size_with_csi() -> Option<(u32, u32)> {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    struct RestoreTermios {
        fd: libc::c_int,
        original: libc::termios,
    }

    impl Drop for RestoreTermios {
        fn drop(&mut self) {
            let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.original) };
        }
    }

    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let fd = tty.as_raw_fd();
    let mut original = std::mem::MaybeUninit::<libc::termios>::zeroed();
    if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
        return None;
    }
    let original = unsafe { original.assume_init() };
    let mut raw = original;
    raw.c_iflag = 0;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return None;
    }
    let _restore = RestoreTermios { fd, original };

    tty.write_all(b"\x1b[16t").ok()?;
    tty.flush().ok()?;

    let deadline = Instant::now() + CSI_CELL_SIZE_TIMEOUT;
    let mut response = Vec::with_capacity(128);
    let mut buffer = [0_u8; 128];
    while Instant::now() < deadline {
        let timeout = deadline.saturating_duration_since(Instant::now());
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe {
            libc::poll(
                &mut descriptor,
                1,
                timeout.as_millis().clamp(1, i32::MAX as u128) as libc::c_int,
            )
        };
        if ready <= 0 {
            break;
        }

        match tty.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                response.extend_from_slice(&buffer[..count]);
                if let Some(size) = parse_csi_16t_response(&response) {
                    return Some(size);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }

    parse_csi_16t_response(&response)
}

#[cfg(not(unix))]
fn query_cell_size_with_csi() -> Option<(u32, u32)> {
    None
}

#[cfg(any(unix, test))]
fn parse_csi_16t_response(data: &[u8]) -> Option<(u32, u32)> {
    const PREFIX: &str = "\x1b[6;";
    let text = std::str::from_utf8(data).ok()?;
    for (start, _) in text.match_indices(PREFIX) {
        let remainder = &text[start + PREFIX.len()..];
        let end = remainder.find('t')?;
        let mut fields = remainder[..end].split(';');
        let height = fields.next()?.parse::<u32>().ok()?;
        let width = fields.next()?.parse::<u32>().ok()?;
        if width > 0 && height > 0 {
            return Some((width, height));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use vivid_sdk::DisplayState;

    #[test]
    fn cell_count_rounds_up() {
        assert_eq!(cells_for_pixels(640, 10), 64);
        assert_eq!(cells_for_pixels(641, 10), 65);
    }

    #[test]
    fn parses_cell_size_response() {
        assert_eq!(parse_csi_16t_response(b"noise\x1b[6;19;9t"), Some((9, 19)));
    }

    #[test]
    fn presenter_geometry_is_accepted_only_after_settling() {
        let mut display = DisplayState {
            display_generation: 2,
            viewport_width: 900,
            viewport_height: 600,
            grid_columns: 90,
            grid_rows: 30,
            cell_width: 10,
            cell_height: 20,
            settled: false,
        };
        assert!(TerminalGeometry::from_settled_display(display).is_none());
        display.settled = true;
        assert_eq!(
            TerminalGeometry::from_settled_display(display),
            Some(TerminalGeometry::with_cell_size(90, 30, 10, 20))
        );
    }
}
