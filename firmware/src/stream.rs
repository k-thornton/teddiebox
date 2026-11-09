//! One story playing while it is still being downloaded.
//!
//! The downloader and the decoder share one file handle, because
//! `embedded-sdmmc` cannot open a file twice, and both run in the media task:
//! the download is serviced between decoded frames. This records who holds
//! the handle and how far the writer has got, so the decoder never reads a
//! page that is not on the card yet and the handle is closed exactly once,
//! by whichever side finishes last.

use core::cell::RefCell;
use critical_section::Mutex as CsMutex;
use embedded_sdmmc::RawFile;
use teddiebox_download::{may_decode, Pages};
use teddiebox_taf::PAGE_SIZE;

/// Pages the writer must be past the body's start before playback begins.
///
/// 64 pages is 256 KB, about twenty seconds at 96 kbps, and the same as the
/// throttle's pause threshold (`PAUSE_ABOVE` in `main.rs`): the download is
/// already pausing when audio starts. Measured with 24: three audio restarts
/// in the first four seconds, while the radio still ran at full speed beside
/// the decoder; none once the throttle had paused it. At the ~100 KB/s seen
/// against teddyCloud this is about 2.5 s after the first byte.
pub const HEAD_START: Pages = Pages(64);

/// Whole pages the writer must be past the one about to be read, so a frame
/// that crosses a page boundary has all its bytes.
const MARGIN: Pages = Pages(2);

#[derive(Clone, Copy)]
struct Stream {
    directory: u32,
    file: u32,
    handle: RawFile,
    /// Where this body starts in the file. Bytes before it were already there.
    at: u32,
    /// Bytes on the card so far, counted from the start of the file.
    written: u32,
    /// How long the whole file is, when the server said.
    total: Option<u32>,
    writer_open: bool,
    reader_open: bool,
    /// The writer finished, and the file is whole.
    complete: bool,
    /// The writer finished short. The decoder may play what there is.
    failed: bool,
    /// The head start has been reported once.
    announced: bool,
}

static STREAM: CsMutex<RefCell<Option<Stream>>> = CsMutex::new(RefCell::new(None));

fn with<R>(f: impl FnOnce(&mut Option<Stream>) -> R) -> R {
    critical_section::with(|cs| f(&mut STREAM.borrow_ref_mut(cs)))
}

/// A download has opened `CONTENT/<directory>/<file>` and will append from `at`.
pub fn begin(directory: u32, file: u32, handle: RawFile, at: u32, total: Option<u32>) {
    with(|s| {
        *s = Some(Stream {
            directory,
            file,
            handle,
            at,
            written: at,
            total,
            writer_open: true,
            reader_open: false,
            complete: false,
            failed: false,
            announced: false,
        })
    });
}

/// The writer has put this many bytes of the body on the card.
pub fn advance(written_this_body: u32) {
    with(|s| {
        if let Some(s) = s {
            s.written = s.at.saturating_add(written_this_body);
        }
    });
}

/// Whether enough of the story is on the card to start playing it.
///
/// Said once per download, so the caller reports it once.
pub fn ready_to_start() -> bool {
    with(|s| {
        let Some(s) = s else { return false };
        if s.announced || !s.writer_open {
            return false;
        }
        let head =
            s.at.saturating_add(HEAD_START.0.saturating_mul(PAGE_SIZE as u32));
        let enough = s.written >= head || s.total.is_some_and(|total| s.written >= total);
        if enough {
            s.announced = true;
        }
        enough
    })
}

/// Whether a download is writing any story right now.
pub fn is_writing() -> bool {
    with(|s| s.is_some_and(|s| s.writer_open))
}

/// Whether a download is writing this story right now.
pub fn is_writing_path(directory: u32, file: u32) -> bool {
    with(|s| s.is_some_and(|s| s.writer_open && (s.directory, s.file) == (directory, file)))
}

/// A figure placed again while its story still downloads: say "enough to
/// start" once more, so the new placement joins the download.
pub fn rearm() {
    with(|s| {
        if let Some(s) = s {
            s.announced = false;
        }
    });
}

/// The writer is finished. Returns whether the caller should close the
/// handle: it should not while a playback still reads from it, and that
/// playback then closes it.
pub fn writer_done(whole: bool) -> bool {
    with(|s| {
        let Some(stream) = s else { return true };
        stream.writer_open = false;
        stream.complete = whole;
        stream.failed = !whole;
        if stream.reader_open {
            false
        } else {
            *s = None;
            true
        }
    })
}

/// A playback of this story borrows the writer's handle, with the file's
/// whole length. `None` when no download has this story open.
pub fn reader_begin(directory: u32, file: u32) -> Option<(RawFile, u32)> {
    with(|s| {
        let stream = s.as_mut()?;
        if (stream.directory, stream.file) != (directory, file) || !stream.writer_open {
            return None;
        }
        stream.reader_open = true;
        Some((stream.handle, stream.total.unwrap_or(stream.written)))
    })
}

/// The playback is finished with the borrowed handle. Returns whether the
/// caller should close it: yes once the writer is gone too.
pub fn reader_done(directory: u32, file: u32) -> bool {
    with(|s| {
        let Some(stream) = s else { return true };
        if (stream.directory, stream.file) != (directory, file) {
            return true;
        }
        stream.reader_open = false;
        if stream.writer_open {
            false
        } else {
            *s = None;
            true
        }
    })
}

/// How far the writer is ahead of the decoder, in whole pages, while a
/// playback reads through the writer's handle. `None` when nothing plays
/// from the download, so the net task fetches at full speed.
pub fn lead() -> Option<Pages> {
    let page = crate::audio::PAGE.load(core::sync::atomic::Ordering::Relaxed);
    with(|s| {
        let stream = s.as_ref()?;
        if !stream.reader_open {
            return None;
        }
        Some(Pages(
            (stream.written / PAGE_SIZE as u32).saturating_sub(page),
        ))
    })
}

/// Whether the decoder may read on from `page`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// The pages it needs are on the card.
    Go,
    /// Not yet; the writer is still going.
    Wait,
    /// Never: the download ended before it got this far.
    Lost,
}

pub fn gate(directory: u32, file: u32, page: u32) -> Gate {
    with(|s| {
        let Some(stream) = s else { return Gate::Go };
        if (stream.directory, stream.file) != (directory, file) {
            return Gate::Go;
        }
        let watermark = Pages(stream.written / PAGE_SIZE as u32);
        if may_decode(Pages(page), watermark, MARGIN, stream.complete) {
            Gate::Go
        } else if stream.writer_open {
            Gate::Wait
        } else {
            Gate::Lost
        }
    })
}
