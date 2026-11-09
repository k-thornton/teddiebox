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
    /// The shared handle, once either side has opened the file.
    handle: Option<RawFile>,
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
    /// Downloads restarted after stopping short under this playback.
    retries: u32,
    /// When the next restart is due, in the box's millisecond clock.
    retry_due_ms: Option<u64>,
    /// A restart has been asked for and its writer has not opened yet.
    retrying: bool,
}

/// How many times a download that stops short under a playing story is
/// restarted before the story is allowed to run dry: 1, 2, 4 and 8 s apart,
/// as a stock box does. Progress resets the count.
const RETRIES: u32 = 4;

static STREAM: CsMutex<RefCell<Option<Stream>>> = CsMutex::new(RefCell::new(None));

fn with<R>(f: impl FnOnce(&mut Option<Stream>) -> R) -> R {
    critical_section::with(|cs| f(&mut STREAM.borrow_ref_mut(cs)))
}

/// A download has opened `CONTENT/<directory>/<file>` and will append from `at`.
pub fn begin(directory: u32, file: u32, handle: RawFile, at: u32, total: Option<u32>) {
    with(|s| {
        // A restart under a playback keeps that playback's handle and its
        // retry count; everything about the writer starts afresh.
        let (handle, reader_open, retries, announced) = match *s {
            Some(held) if (held.directory, held.file) == (directory, file) && held.reader_open => {
                (held.handle.or(Some(handle)), true, held.retries, true)
            }
            _ => (Some(handle), false, 0, false),
        };
        *s = Some(Stream {
            directory,
            file,
            handle,
            at,
            written: at,
            total,
            writer_open: true,
            reader_open,
            complete: false,
            failed: false,
            announced,
            retries,
            retry_due_ms: None,
            retrying: false,
        })
    });
}

/// The network could not be reached for a story the card holds part of:
/// the partial plays, gated at its end, and the download is tried again
/// shortly (`schedule_retry`). The playback opens the file itself and
/// hands the handle over with [`adopt_reader`].
pub fn begin_offline(directory: u32, file: u32, written: u32, total: Option<u32>) {
    with(|s| {
        *s = Some(Stream {
            directory,
            file,
            handle: None,
            at: written,
            written,
            total,
            writer_open: false,
            reader_open: false,
            complete: false,
            failed: true,
            announced: true,
            retries: 0,
            retry_due_ms: None,
            retrying: false,
        })
    });
}

/// A playback opened this story's file itself. If the story is a partial
/// waiting for its download (see [`begin_offline`]), the handle is shared
/// from now on. Returns whether it was.
pub fn adopt_reader(directory: u32, file: u32, handle: RawFile) -> bool {
    with(|s| {
        let Some(stream) = s.as_mut() else {
            return false;
        };
        if (stream.directory, stream.file) != (directory, file) || stream.writer_open {
            return false;
        }
        stream.handle = Some(handle);
        stream.reader_open = true;
        true
    })
}

/// Whether a partial is playing with no download running for it.
pub fn is_reading_offline() -> bool {
    with(|s| s.is_some_and(|s| s.reader_open && !s.writer_open))
}

/// How much of this story is on the card, while a download or playback
/// holds its file open and the card cannot be asked.
pub fn written_for(directory: u32, file: u32) -> Option<u32> {
    with(|s| {
        s.filter(|s| (s.directory, s.file) == (directory, file))
            .map(|s| s.written)
    })
}

/// The handle a playback of this story still reads through, for a download
/// restarting under it: the card cannot open the file a second time.
pub fn handle_for_resume(directory: u32, file: u32) -> Option<RawFile> {
    with(|s| {
        s.filter(|s| (s.directory, s.file) == (directory, file) && s.reader_open && !s.writer_open)
            .and_then(|s| s.handle)
    })
}

/// The writer stopped short under a playback. Returns how long to wait
/// before restarting it, or `None` once the retries are used up.
pub fn schedule_retry(now_ms: u64) -> Option<u64> {
    with(|s| {
        let s = s.as_mut()?;
        if !s.reader_open || s.retries >= RETRIES {
            return None;
        }
        let delay = 1_000u64 << s.retries;
        s.retries += 1;
        s.retry_due_ms = Some(now_ms.saturating_add(delay));
        s.retrying = false;
        Some(delay)
    })
}

/// Whether a scheduled restart is due now. Answers yes once.
pub fn take_due_retry(now_ms: u64) -> bool {
    with(|s| {
        let Some(s) = s else { return false };
        match s.retry_due_ms {
            Some(due) if now_ms >= due => {
                s.retry_due_ms = None;
                s.retrying = true;
                true
            }
            _ => false,
        }
    })
}

/// No more restarts: the decoder may run dry at the end of what landed.
pub fn stop_retrying() {
    with(|s| {
        if let Some(s) = s {
            s.retry_due_ms = None;
            s.retrying = false;
            s.retries = RETRIES;
        }
    });
}

/// The writer has put this many bytes of the body on the card.
pub fn advance(written_this_body: u32) {
    with(|s| {
        if let Some(s) = s {
            s.written = s.at.saturating_add(written_this_body);
            if written_this_body > 0 {
                // Bytes are arriving again: a later stop starts the count over.
                s.retries = 0;
            }
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
        let handle = stream.handle?;
        stream.reader_open = true;
        Some((handle, stream.total.unwrap_or(stream.written)))
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
        } else if stream.writer_open || stream.retry_due_ms.is_some() || stream.retrying {
            // Still writing, about to start again, or starting again now.
            Gate::Wait
        } else {
            Gate::Lost
        }
    })
}
