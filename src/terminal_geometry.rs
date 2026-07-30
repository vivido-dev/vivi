use std::io::{self, Write};
use std::time::{Duration, Instant};

use vivid_protocol::messages::PayloadMap;
use vivid_protocol::{cbor::Value, registry};
use vivid_sdk::{Fit, RequestMetadata, SceneNode, SessionEvent, Surface};

use crate::client::VividClient;

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

    pub fn from_target_descriptor(descriptor: &PayloadMap) -> io::Result<Self> {
        if descriptor.len() != 9
            || descriptor
                .iter()
                .enumerate()
                .any(|(index, (key, _))| *key != index as u64)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "terminal target descriptor must contain exactly keys 0 through 8",
            ));
        }
        let unsigned = |key: usize| {
            descriptor[key].1.as_u64().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("terminal target descriptor key {key} is not unsigned"),
                )
            })
        };
        let cols = u16::try_from(unsigned(2)?)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "grid columns exceed u16"))?;
        let rows = u16::try_from(unsigned(3)?)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "grid rows exceed u16"))?;
        let cell_width = u32::try_from(unsigned(4)?)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "cell width exceeds u32"))?;
        let cell_height = u32::try_from(unsigned(5)?)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "cell height exceeds u32"))?;
        let settled = descriptor[6].1.as_bool().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "terminal target descriptor settled flag is not boolean",
            )
        })?;
        if unsigned(0)? == 0
            || unsigned(1)? == 0
            || cols == 0
            || rows == 0
            || cell_width == 0
            || cell_height == 0
            || unsigned(7)? != 3
            || unsigned(8)? == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "terminal target descriptor contains invalid dimensions or anchor capabilities",
            ));
        }
        if !settled {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "terminal target geometry is not settled",
            ));
        }
        Ok(Self::with_cell_size(cols, rows, cell_width, cell_height))
    }

    pub fn settled_presenter(client: &mut VividClient) -> Self {
        let deadline = Instant::now() + DISPLAY_SETTLE_TIMEOUT;
        loop {
            match Self::from_target_descriptor(&client.info().target_descriptor) {
                Ok(geometry) => return geometry,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return Self::current(),
            }
            while let Ok(Some(event)) = client.take_event() {
                if let SessionEvent::TargetChanged(payload) = event
                    && client.apply_target_changed(&payload).is_err()
                {
                    return Self::current();
                }
            }
            if Instant::now() >= deadline {
                return Self::current();
            }
            std::thread::sleep(Duration::from_millis(5));
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

pub fn place_surface(
    client: &mut VividClient,
    surface: &Surface,
    node_id: u64,
    columns: u32,
    rows: u32,
) -> io::Result<()> {
    let width = fixed_cells(columns)?;
    let height = fixed_cells(rows)?;
    let trusted_anchor_path = std::env::var_os("TMUX").is_none()
        && std::env::var_os("STY").is_none()
        && client.info().target_descriptor[7].1.as_u64() == Some(3);
    if !trusted_anchor_path {
        client.place_terminal_surface(surface, node_id, 0, 0, width, height, 1)?;
        return Ok(());
    }

    let context_id = client.info().root_context_id;
    let anchor_id = random_nonzero_u64()?;
    let marker = if std::env::var_os("VIVID_ANCHOR_TRANSPORT").as_deref()
        == Some(std::ffi::OsStr::new("conpty"))
    {
        client.conpty_anchor_marker(context_id, anchor_id)?
    } else {
        client.anchor_marker(context_id, anchor_id)?
    };
    if !client.is_offline() {
        let mut stdout = io::stdout().lock();
        stdout.write_all(marker.as_bytes())?;
        stdout.flush()?;
        wait_for_anchor(client, context_id, anchor_id)?;
    }

    let node = SceneNode {
        owning_context_id: context_id,
        node_id,
        surface_context_id: surface.context_id(),
        surface_id: surface.id(),
        geometry: vec![
            (0, Value::Unsigned(2)),
            (1, signed(0)),
            (2, signed(0)),
            (3, signed(width)),
            (4, signed(height)),
            (5, Value::Unsigned(1)),
            (6, Value::Unsigned(context_id)),
            (7, Value::Unsigned(anchor_id)),
        ],
        fit: Fit::Contain,
        linear_sampling: true,
        z_index: 0,
        visible: true,
        opacity: u16::MAX,
        clip: None,
    };
    create_node_with_target_retry(client, &node)
}

fn create_node_with_target_retry(client: &mut VividClient, node: &SceneNode) -> io::Result<()> {
    match client.create_node(node, &RequestMetadata::default()) {
        Ok(_) => Ok(()),
        Err(error) if presenter_code(&error) == Some(registry::error::STALE_TARGET_GENERATION) => {
            let mut applied = false;
            while let Some(event) = client.take_event()? {
                if let SessionEvent::TargetChanged(payload) = event {
                    client.apply_target_changed(&payload)?;
                    applied = true;
                }
            }
            if !applied {
                return Err(error);
            }
            client
                .create_node(node, &RequestMetadata::default())
                .map(|_| ())
        }
        Err(error) => Err(error),
    }
}

fn wait_for_anchor(client: &mut VividClient, context_id: u64, anchor_id: u64) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        while let Some(event) = client.take_event()? {
            match event {
                SessionEvent::AnchorReady {
                    context_id: event_context,
                    anchor_id: event_anchor,
                    ..
                } if event_context == context_id && event_anchor == anchor_id => return Ok(()),
                SessionEvent::TargetChanged(payload) => {
                    client.apply_target_changed(&payload)?;
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "presenter did not acknowledge the terminal anchor",
    ))
}

fn presenter_code(error: &io::Error) -> Option<u64> {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<vivid_sdk::PresenterError>())
        .map(|error| error.code)
}

fn fixed_cells(cells: u32) -> io::Result<i64> {
    i64::from(cells)
        .checked_shl(32)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cell geometry overflows"))
}

fn signed(value: i64) -> Value {
    if value >= 0 {
        Value::Unsigned(value as u64)
    } else {
        Value::Negative(value)
    }
}

fn random_nonzero_u64() -> io::Result<u64> {
    loop {
        let mut bytes = [0_u8; 8];
        getrandom::fill(&mut bytes).map_err(io::Error::other)?;
        let value = u64::from_be_bytes(bytes);
        if value != 0 {
            return Ok(value);
        }
    }
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
        let mut descriptor = vec![
            (0, Value::Unsigned(900)),
            (1, Value::Unsigned(600)),
            (2, Value::Unsigned(90)),
            (3, Value::Unsigned(30)),
            (4, Value::Unsigned(10)),
            (5, Value::Unsigned(20)),
            (6, Value::Bool(false)),
            (7, Value::Unsigned(3)),
            (8, Value::Unsigned(64)),
        ];
        assert_eq!(
            TerminalGeometry::from_target_descriptor(&descriptor)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        descriptor[6].1 = Value::Bool(true);
        assert_eq!(
            TerminalGeometry::from_target_descriptor(&descriptor).unwrap(),
            TerminalGeometry::with_cell_size(90, 30, 10, 20)
        );
    }
}
