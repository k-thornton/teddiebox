//! The one story that may be playing while it downloads, shared between the
//! download servicing and the decoder in the media task.
//!
//! The bookkeeping itself is [`teddiebox_download::Stream`], tested on the
//! host; this holds the firmware's single instance behind a critical section
//! and gives each side the small call it needs. The handle is the card
//! library's, which cannot open a file twice, so both sides share it.

use core::cell::RefCell;
use critical_section::Mutex as CsMutex;
use embedded_sdmmc::RawFile;
use teddiebox_download::{Handle, Pages, Stream};

pub use teddiebox_download::{Gate, HEAD_START};

static STREAM: CsMutex<RefCell<Option<Stream<RawFile>>>> = CsMutex::new(RefCell::new(None));

fn with<R>(f: impl FnOnce(&mut Option<Stream<RawFile>>) -> R) -> R {
    critical_section::with(|cs| f(&mut STREAM.borrow_ref_mut(cs)))
}

/// A download has opened `CONTENT/<directory>/<file>` and will append from `at`.
pub fn begin(directory: u32, file: u32, handle: RawFile, at: u32, total: Option<u32>) {
    with(|s| *s = Some(Stream::begin(*s, directory, file, handle, at, total)));
}

/// The network could not be reached for a story the card holds part of:
/// the partial plays, gated at its end, and the download is tried again
/// shortly (`schedule_retry`).
pub fn begin_offline(directory: u32, file: u32, written: u32, total: Option<u32>) {
    with(|s| *s = Some(Stream::begin_offline(directory, file, written, total)));
}

/// A partial waiting for its download, which no one has opened yet, and the
/// file's whole length: the playback opens it for writing too.
pub fn awaiting_reader(directory: u32, file: u32) -> Option<u32> {
    with(|s| s.as_ref()?.awaiting_reader(directory, file))
}

/// A playback opened this story's file itself; share the handle if the story
/// is a partial waiting for its download. Returns whether it was.
pub fn adopt_reader(directory: u32, file: u32, handle: RawFile) -> bool {
    with(|s| {
        s.as_mut()
            .is_some_and(|s| s.adopt_reader(directory, file, handle))
    })
}

/// Whether a partial is playing with no download running for it.
pub fn is_reading_offline() -> bool {
    with(|s| s.is_some_and(|s| s.is_reading_offline()))
}

/// Whether the server already confirmed this file is its version this session.
pub fn version_verified(directory: u32, file: u32) -> bool {
    with(|s| s.is_some_and(|s| s.version_verified(directory, file)))
}

/// How much of this story is on the card while its file is held open.
pub fn written_for(directory: u32, file: u32) -> Option<u32> {
    with(|s| s.as_ref()?.written_for(directory, file))
}

/// The handle a playback of this story still reads through, for a download
/// restarting under it.
pub fn handle_for_resume(directory: u32, file: u32) -> Option<RawFile> {
    with(|s| s.as_ref()?.handle_for_resume(directory, file))
}

/// The writer stopped short under a playback, or could not start. How long
/// to wait before trying again, or `None` when there is nothing to retry for.
pub fn schedule_retry(now_ms: u64) -> Option<u64> {
    with(|s| s.as_mut()?.schedule_retry(now_ms))
}

/// Whether a scheduled restart is due now. Answers yes once.
pub fn take_due_retry(now_ms: u64) -> bool {
    with(|s| s.as_mut().is_some_and(|s| s.take_due_retry(now_ms)))
}

/// No more restarts: the decoder stops at the end of what landed.
pub fn stop_retrying() {
    with(|s| {
        if let Some(s) = s {
            s.stop_retrying();
        }
    });
}

/// The writer has put this many bytes of the body on the card.
pub fn advance(written_this_body: u32) {
    with(|s| {
        if let Some(s) = s {
            s.advance(written_this_body);
        }
    });
}

/// Whether enough of the story is on the card to start playing it. Said once.
pub fn ready_to_start() -> bool {
    with(|s| s.as_mut().is_some_and(|s| s.ready_to_start()))
}

/// Whether a download is writing any story right now.
pub fn is_writing() -> bool {
    with(|s| s.is_some_and(|s| s.is_writing()))
}

/// Whether a download is writing this story right now.
pub fn is_writing_path(directory: u32, file: u32) -> bool {
    with(|s| s.is_some_and(|s| s.is_writing_path(directory, file)))
}

/// A figure placed again while its story still downloads: announce the
/// start once more, so the new placement joins the download.
pub fn rearm() {
    with(|s| {
        if let Some(s) = s {
            s.rearm();
        }
    });
}

/// The writer is finished. Returns whether the caller should close the
/// handle now; otherwise the playback still reading through it closes it.
pub fn writer_done(whole: bool) -> bool {
    with(|s| match s {
        None => true,
        Some(stream) => match stream.writer_done(whole) {
            Handle::Close => {
                *s = None;
                true
            }
            Handle::Keep => false,
        },
    })
}

/// A playback of this story borrows the writer's handle, with the file's
/// whole length. `None` when no download has this story open.
pub fn reader_begin(directory: u32, file: u32) -> Option<(RawFile, u32)> {
    with(|s| s.as_mut()?.reader_begin(directory, file))
}

/// The playback is finished with the handle. Returns whether the caller
/// should close it: yes once the writer is gone too.
pub fn reader_done(directory: u32, file: u32) -> bool {
    with(|s| match s {
        None => true,
        Some(stream) => match stream.reader_done(directory, file) {
            Handle::Close => {
                if stream.is_over() {
                    *s = None;
                }
                true
            }
            Handle::Keep => false,
        },
    })
}

/// How far the writer is ahead of the decoder, in whole pages, while a
/// playback reads through the writer's handle. `None` when nothing plays
/// from the download, so the net task fetches at full speed.
pub fn lead() -> Option<Pages> {
    let page = crate::audio::PAGE.load(core::sync::atomic::Ordering::Relaxed);
    with(|s| s.as_ref()?.lead(page))
}

/// Whether the decoder may read on from `page`.
pub fn gate(directory: u32, file: u32, page: u32) -> Gate {
    with(|s| {
        s.as_ref()
            .map_or(Gate::Go, |s| s.gate(directory, file, page))
    })
}
