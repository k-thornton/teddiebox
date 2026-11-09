//! One story playing while it is still being downloaded: who holds the
//! file, how far the writer has got, and whether the decoder may read on.
//!
//! The downloader and the decoder share one file handle, because the card
//! library cannot open a file twice. This is the pure bookkeeping of that
//! sharing, with the handle as a type parameter so it can be tested here;
//! the firmware keeps one `Stream` behind a critical section.
//!
//! Lifecycle: a download [`begin`](Stream::begin)s, the decoder
//! [`reader_begin`](Stream::reader_begin)s through its handle once enough is
//! on the card ([`ready_to_start`](Stream::ready_to_start)), and whichever
//! side finishes last closes the handle ([`writer_done`](Stream::writer_done),
//! [`reader_done`](Stream::reader_done)). A download that stops short under
//! its playback is tried again ([`schedule_retry`](Stream::schedule_retry));
//! a story the network could not be reached for starts the other way round
//! ([`begin_offline`](Stream::begin_offline), [`adopt_reader`](Stream::adopt_reader)).

use crate::units::Pages;
use crate::window::may_decode;
use teddiebox_taf::PAGE_SIZE;

/// Pages the writer must be past the body's start before playback begins.
///
/// 64 pages is 256 KB, about twenty seconds at 96 kbps, and the same as the
/// firmware's throttle pause threshold: the download is already pausing when
/// audio starts. Measured with 24: three audio restarts in the first four
/// seconds, while the radio still ran at full speed beside the decoder; none
/// once the throttle had paused it.
pub const HEAD_START: Pages = Pages(64);

/// Whole pages the writer must be past the one about to be read, so a frame
/// that crosses a page boundary has all its bytes.
const MARGIN: Pages = Pages(2);

/// The longest wait between restarts: 1, 2, 4, then 8 s and 8 s again, for
/// as long as the story plays, as a stock box does. Progress resets it.
const MAX_BACKOFF_SHIFT: u32 = 3;

/// Whether the decoder may read on from a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// The pages it needs are on the card.
    Go,
    /// Not yet; the writer is still going.
    Wait,
    /// The download is not delivering and the decoder has caught up with
    /// it: the story stops here.
    Lost,
}

/// What to do with the handle once one side is finished with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    /// Nobody else holds it: close it, and the stream is over.
    Close,
    /// The other side still reads or writes through it and closes it later.
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stream<H: Copy> {
    directory: u32,
    file: u32,
    /// The shared handle, once either side has opened the file.
    handle: Option<H>,
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
    /// The head start has been reported once.
    announced: bool,
    /// Downloads restarted after stopping short under this playback.
    retries: u32,
    /// When the next restart is due, in milliseconds of the box's clock.
    retry_due_ms: Option<u64>,
    /// A restart has been asked for and its writer has not opened yet.
    retrying: bool,
    /// No more restarts: the story changed under the playback.
    given_up: bool,
    /// The server has confirmed this file is its version, by a download
    /// that started from it this session. A retry need not ask again.
    verified: bool,
}

impl<H: Copy> Stream<H> {
    fn is(&self, directory: u32, file: u32) -> bool {
        (self.directory, self.file) == (directory, file)
    }

    /// A download has opened `CONTENT/<directory>/<file>` and will append
    /// from `at`. `before` is the stream this replaces, if any: a restart
    /// under a playback keeps that playback's handle and retry count, and
    /// everything about the writer starts afresh.
    pub fn begin(
        before: Option<Self>,
        directory: u32,
        file: u32,
        handle: H,
        at: u32,
        total: Option<u32>,
    ) -> Self {
        let (handle, reader_open, retries, announced) = match before {
            Some(held) if held.is(directory, file) && held.reader_open => {
                (held.handle.or(Some(handle)), true, held.retries, true)
            }
            _ => (Some(handle), false, 0, false),
        };
        Stream {
            directory,
            file,
            handle,
            at,
            written: at,
            total,
            writer_open: true,
            reader_open,
            complete: false,
            announced,
            retries,
            retry_due_ms: None,
            retrying: false,
            given_up: false,
            verified: true,
        }
    }

    /// The network could not be reached for a story the card holds part
    /// of: the partial plays, gated at its end, and the download is tried
    /// again shortly ([`schedule_retry`](Self::schedule_retry)). The
    /// playback opens the file itself and hands the handle over with
    /// [`adopt_reader`](Self::adopt_reader).
    pub fn begin_offline(directory: u32, file: u32, written: u32, total: Option<u32>) -> Self {
        Stream {
            directory,
            file,
            handle: None,
            at: written,
            written,
            total,
            writer_open: false,
            reader_open: false,
            complete: false,
            announced: true,
            retries: 0,
            retry_due_ms: None,
            retrying: false,
            given_up: false,
            verified: false,
        }
    }

    /// A partial waiting for its download, which no one has opened yet: the
    /// playback about to open it must do so for writing too, and this is
    /// the file's whole length.
    pub fn awaiting_reader(&self, directory: u32, file: u32) -> Option<u32> {
        (self.is(directory, file)
            && !self.writer_open
            && !self.reader_open
            && self.handle.is_none())
        .then_some(self.total.unwrap_or(self.written))
    }

    /// A playback opened this story's file itself. If the story is a partial
    /// waiting for its download, the handle is shared from now on. Returns
    /// whether it was.
    pub fn adopt_reader(&mut self, directory: u32, file: u32, handle: H) -> bool {
        if !self.is(directory, file) || self.writer_open {
            return false;
        }
        self.handle = Some(handle);
        self.reader_open = true;
        true
    }

    /// Whether a partial is playing with no download running for it.
    pub fn is_reading_offline(&self) -> bool {
        self.reader_open && !self.writer_open
    }

    /// Whether the server has already confirmed this file is its version
    /// this session, so a download continuing it need not ask again.
    pub fn version_verified(&self, directory: u32, file: u32) -> bool {
        self.is(directory, file) && self.verified
    }

    /// How much of this story is on the card, while a download or playback
    /// holds its file open and the card cannot be asked.
    pub fn written_for(&self, directory: u32, file: u32) -> Option<u32> {
        self.is(directory, file).then_some(self.written)
    }

    /// The handle a playback of this story still reads through, for a
    /// download restarting under it.
    pub fn handle_for_resume(&self, directory: u32, file: u32) -> Option<H> {
        (self.is(directory, file) && self.reader_open && !self.writer_open)
            .then_some(self.handle)
            .flatten()
    }

    /// The writer stopped short under a playback, or could not start.
    /// Returns how long to wait before trying again, or `None` when nothing
    /// plays from this story or the story changed under it.
    pub fn schedule_retry(&mut self, now_ms: u64) -> Option<u64> {
        self.retrying = false;
        if !self.reader_open || self.given_up {
            return None;
        }
        let delay = 1_000u64 << self.retries.min(MAX_BACKOFF_SHIFT);
        self.retries += 1;
        self.retry_due_ms = Some(now_ms.saturating_add(delay));
        Some(delay)
    }

    /// Whether a scheduled restart is due now. Answers yes once.
    pub fn take_due_retry(&mut self, now_ms: u64) -> bool {
        match self.retry_due_ms {
            Some(due) if now_ms >= due => {
                self.retry_due_ms = None;
                self.retrying = true;
                true
            }
            _ => false,
        }
    }

    /// No more restarts: the decoder stops at the end of what landed.
    pub fn stop_retrying(&mut self) {
        self.retry_due_ms = None;
        self.retrying = false;
        self.given_up = true;
    }

    /// The writer has put this many bytes of the body on the card.
    pub fn advance(&mut self, written_this_body: u32) {
        self.written = self.at.saturating_add(written_this_body);
        if written_this_body > 0 {
            // Bytes are arriving again: a later stop starts the count over.
            self.retries = 0;
        }
    }

    /// Whether enough of the story is on the card to start playing it.
    /// Said once per download, so the caller reports it once.
    pub fn ready_to_start(&mut self) -> bool {
        if self.announced || !self.writer_open {
            return false;
        }
        let head = self
            .at
            .saturating_add(HEAD_START.0.saturating_mul(PAGE_SIZE as u32));
        let enough = self.written >= head || self.total.is_some_and(|total| self.written >= total);
        if enough {
            self.announced = true;
        }
        enough
    }

    /// Whether the download is writing right now.
    pub fn is_writing(&self) -> bool {
        self.writer_open
    }

    /// Whether the download is writing this story right now.
    pub fn is_writing_path(&self, directory: u32, file: u32) -> bool {
        self.writer_open && self.is(directory, file)
    }

    /// A figure placed again while its story still downloads: say "enough
    /// to start" once more, so the new placement joins the download.
    pub fn rearm(&mut self) {
        self.announced = false;
    }

    /// The writer is finished. Says whether to close the handle now.
    pub fn writer_done(&mut self, whole: bool) -> Handle {
        self.writer_open = false;
        self.complete = whole;
        if self.reader_open {
            Handle::Keep
        } else {
            Handle::Close
        }
    }

    /// A playback of this story borrows the writer's handle, with the
    /// file's whole length. `None` when no download has this story open.
    pub fn reader_begin(&mut self, directory: u32, file: u32) -> Option<(H, u32)> {
        if !self.is(directory, file) || !self.writer_open {
            return None;
        }
        let handle = self.handle?;
        self.reader_open = true;
        Some((handle, self.total.unwrap_or(self.written)))
    }

    /// The playback is finished with the handle. Says whether to close it
    /// now; `Keep` while the writer still has it. Another story's playback
    /// closes its own handle.
    pub fn reader_done(&mut self, directory: u32, file: u32) -> Handle {
        if !self.is(directory, file) {
            return Handle::Close;
        }
        self.reader_open = false;
        if self.writer_open {
            Handle::Keep
        } else {
            Handle::Close
        }
    }

    /// Whether this stream is over: nobody holds the handle any more.
    pub fn is_over(&self) -> bool {
        !self.writer_open && !self.reader_open
    }

    /// How far the writer is ahead of the decoder at `page`, in whole pages,
    /// while a playback reads through the writer's handle. `None` when
    /// nothing plays from the download, so the fetch runs at full speed.
    pub fn lead(&self, page: u32) -> Option<Pages> {
        self.reader_open
            .then(|| Pages((self.written / PAGE_SIZE as u32).saturating_sub(page)))
    }

    /// Whether the decoder may read on from `page`.
    pub fn gate(&self, directory: u32, file: u32, page: u32) -> Gate {
        if !self.is(directory, file) {
            return Gate::Go;
        }
        let watermark = Pages(self.written / PAGE_SIZE as u32);
        if may_decode(Pages(page), watermark, MARGIN, self.complete) {
            Gate::Go
        } else if self.writer_open {
            // Still writing; the bytes are on their way.
            Gate::Wait
        } else {
            // Nothing is writing: the decoder has caught a download that is
            // down, whether or not a restart is scheduled. A stock box stops
            // here too, and the next placement resumes the partial.
            Gate::Lost
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: u32 = PAGE_SIZE as u32;
    const DIR: u32 = 0x3877_8B2A;
    const FILE: u32 = 0x5301_04E0;
    const TOTAL: u32 = 22_712_320;

    fn fresh() -> Stream<u8> {
        Stream::begin(None, DIR, FILE, 7, 0, Some(TOTAL))
    }

    #[test]
    fn playback_starts_once_the_head_start_is_on_the_card_and_is_said_once() {
        // Given
        let mut s = fresh();

        // When
        s.advance(HEAD_START.0 * PAGE - 1);
        let early = s.ready_to_start();
        s.advance(HEAD_START.0 * PAGE);
        let enough = s.ready_to_start();
        let again = s.ready_to_start();

        // Then
        assert!(!early);
        assert!(enough);
        assert!(!again);
    }

    #[test]
    fn a_short_story_starts_when_it_is_whole() {
        // Given: 100 KB in all, less than the head start
        let mut s = Stream::<u8>::begin(None, DIR, FILE, 7, 0, Some(100 * 1024));

        // When
        s.advance(100 * 1024);

        // Then
        assert!(s.ready_to_start());
    }

    #[test]
    fn the_decoder_waits_for_the_writer_and_reads_what_has_landed() {
        // Given: 70 pages written, the reader borrowing the handle
        let mut s = fresh();
        s.advance(70 * PAGE);
        let (handle, total) = s.reader_begin(DIR, FILE).unwrap();

        // When / Then
        assert_eq!((handle, total), (7, TOTAL));
        assert_eq!(s.gate(DIR, FILE, 10), Gate::Go);
        assert_eq!(s.gate(DIR, FILE, 69), Gate::Wait);
        assert_eq!(s.gate(DIR, 1, 69), Gate::Go, "another story is not gated");
        assert_eq!(s.lead(10), Some(Pages(60)));
    }

    #[test]
    fn whichever_side_finishes_last_closes_the_handle() {
        // Given
        let mut s = fresh();
        s.advance(70 * PAGE);
        s.reader_begin(DIR, FILE).unwrap();

        // When: the writer finishes first
        let writer = s.writer_done(true);
        let after_reader = s.reader_done(DIR, FILE);

        // Then
        assert_eq!(writer, Handle::Keep);
        assert_eq!(after_reader, Handle::Close);
        assert!(s.is_over());
    }

    #[test]
    fn a_complete_download_lets_the_decoder_read_to_the_end() {
        // Given
        let mut s = fresh();
        s.advance(TOTAL);
        s.reader_begin(DIR, FILE).unwrap();
        s.writer_done(true);

        // When / Then: the last page is readable with no margin beyond it
        assert_eq!(s.gate(DIR, FILE, TOTAL / PAGE - 1), Gate::Go);
    }

    #[test]
    fn a_download_that_stops_short_is_retried_with_backoff_while_the_story_plays() {
        // Given
        let mut s = fresh();
        s.advance(100 * PAGE);
        s.reader_begin(DIR, FILE).unwrap();
        s.writer_done(false);

        // When
        let delays: [Option<u64>; 6] =
            core::array::from_fn(|i| s.schedule_retry(i as u64 * 10_000));

        // Then: 1, 2, 4, 8, 8, 8 s, never giving up
        assert_eq!(
            delays,
            [
                Some(1_000),
                Some(2_000),
                Some(4_000),
                Some(8_000),
                Some(8_000),
                Some(8_000)
            ]
        );
    }

    #[test]
    fn a_retry_fires_once_when_due_and_bytes_arriving_reset_the_backoff() {
        // Given
        let mut s = fresh();
        s.advance(100 * PAGE);
        s.reader_begin(DIR, FILE).unwrap();
        s.writer_done(false);
        s.schedule_retry(1_000);
        s.schedule_retry(2_000);

        // When: the second schedule, at 2 s with a 2 s wait, is due at 4 s
        let early = s.take_due_retry(3_000);
        let due = s.take_due_retry(4_000);
        let twice = s.take_due_retry(5_000);
        // The restarted writer brings bytes.
        let mut restarted = Stream::begin(Some(s), DIR, FILE, 9, 100 * PAGE, Some(TOTAL));
        restarted.advance(PAGE);
        let next = restarted.writer_done(false);
        let delay = restarted.schedule_retry(10_000);

        // Then
        assert!(!early);
        assert!(due);
        assert!(!twice);
        assert_eq!(next, Handle::Keep);
        assert_eq!(delay, Some(1_000), "progress started the backoff over");
    }

    #[test]
    fn a_restart_under_the_playback_keeps_its_handle_and_is_not_announced_again() {
        // Given: the reader holds handle 7, the writer is gone
        let mut s = fresh();
        s.advance(100 * PAGE);
        s.reader_begin(DIR, FILE).unwrap();
        s.writer_done(false);

        // When
        let handle = s.handle_for_resume(DIR, FILE);
        let mut restarted = Stream::begin(Some(s), DIR, FILE, 7, 100 * PAGE, Some(TOTAL));
        restarted.advance(HEAD_START.0 * PAGE);

        // Then
        assert_eq!(handle, Some(7));
        assert!(!restarted.ready_to_start(), "the story already plays");
        assert!(restarted.version_verified(DIR, FILE));
        assert_eq!(restarted.gate(DIR, FILE, 101), Gate::Go);
    }

    #[test]
    fn no_retry_without_a_playback_or_after_the_story_changed() {
        // Given: a plain download, nobody reading
        let mut s = fresh();
        s.writer_done(false);
        let alone = s.schedule_retry(0);

        // And: a playback whose story changed on the server
        let mut t = fresh();
        t.advance(100 * PAGE);
        t.reader_begin(DIR, FILE).unwrap();
        t.writer_done(false);
        t.stop_retrying();

        // When / Then
        assert_eq!(alone, None);
        assert_eq!(t.schedule_retry(0), None);
        assert_eq!(t.gate(DIR, FILE, 100), Gate::Lost);
    }

    #[test]
    fn the_decoder_catching_a_dead_download_is_lost_even_with_a_retry_pending() {
        // Given
        let mut s = fresh();
        s.advance(100 * PAGE);
        s.reader_begin(DIR, FILE).unwrap();
        s.writer_done(false);
        s.schedule_retry(0);

        // When / Then
        assert_eq!(s.gate(DIR, FILE, 50), Gate::Go);
        assert_eq!(s.gate(DIR, FILE, 99), Gate::Lost);
    }

    #[test]
    fn a_partial_played_offline_is_opened_by_the_playback_and_then_shared() {
        // Given: 2 MB of a story on the card, no network
        let mut s = Stream::<u8>::begin_offline(DIR, FILE, 512 * PAGE, Some(TOTAL));

        // When
        let awaiting = s.awaiting_reader(DIR, FILE);
        let adopted = s.adopt_reader(DIR, FILE, 3);
        let delay = s.schedule_retry(0);
        s.take_due_retry(1_000);
        let for_resume = s.handle_for_resume(DIR, FILE);
        let restarted = Stream::begin(Some(s), DIR, FILE, 3, 512 * PAGE, Some(TOTAL));

        // Then
        assert_eq!(awaiting, Some(TOTAL));
        assert!(adopted);
        assert!(s.is_reading_offline());
        assert!(
            !s.version_verified(DIR, FILE),
            "nobody has asked the server yet"
        );
        assert_eq!(delay, Some(1_000));
        assert_eq!(for_resume, Some(3));
        assert!(restarted.is_writing_path(DIR, FILE));
        assert_eq!(restarted.handle_for_resume(DIR, FILE), None);
    }

    #[test]
    fn a_partial_played_offline_is_gated_at_its_end() {
        // Given
        let mut s = Stream::<u8>::begin_offline(DIR, FILE, 512 * PAGE, Some(TOTAL));
        s.adopt_reader(DIR, FILE, 3);

        // When / Then
        assert_eq!(s.gate(DIR, FILE, 400), Gate::Go);
        assert_eq!(s.gate(DIR, FILE, 511), Gate::Lost);
        assert_eq!(s.written_for(DIR, FILE), Some(512 * PAGE));
    }

    #[test]
    fn placing_the_figure_again_rearms_the_start_announcement() {
        // Given
        let mut s = fresh();
        s.advance(HEAD_START.0 * PAGE);
        assert!(s.ready_to_start());

        // When
        s.rearm();

        // Then
        assert!(s.ready_to_start());
    }
}
