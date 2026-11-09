//! What the reducer is allowed to know about the card.
//!
//! `ContentIndex` is synchronous and cannot fail, and only the task that owns
//! the card can answer it directly. So this lives in the media task and only
//! reads.

use teddiebox_core::position::{self, MAX_POSITION};
use teddiebox_core::{ContentIndex, Position, TagUid};
use teddiebox_download::{content_path, is_stock, playable_now, Cached, Sidecar, MAX_SIDECAR};

use core::cell::{Cell, RefCell};

use crate::storage;

/// One path's answer, kept for as long as the index lives.
type Memo<T> = Option<((u32, u32), T)>;

/// Lives for one event (see `apply`), during which nothing writes the story
/// it is asked about. So each card lookup is done once and remembered: one
/// placement asks the same questions several times, and each is a blocking
/// directory scan.
pub struct CardIndex<'a> {
    card: &'a storage::Mounted,
    length: Cell<Memo<Option<u32>>>,
    sidecar_present: Cell<Memo<bool>>,
    cached: RefCell<Memo<Cached>>,
}

impl<'a> CardIndex<'a> {
    pub fn new(card: &'a storage::Mounted) -> Self {
        Self {
            card,
            length: Cell::new(None),
            sidecar_present: Cell::new(None),
            cached: RefCell::new(None),
        }
    }

    fn audio_length(&self, directory: u32, file: u32) -> Option<u32> {
        let key = (directory, file);
        if let Some((seen, length)) = self.length.get() {
            if seen == key {
                return length;
            }
        }
        let length = self.card.audio_length(directory, file);
        self.length.set(Some((key, length)));
        length
    }

    fn has_sidecar(&self, directory: u32, file: u32) -> bool {
        let key = (directory, file);
        if let Some((seen, present)) = self.sidecar_present.get() {
            if seen == key {
                return present;
            }
        }
        let present = self.card.has_sidecar(directory, file);
        self.sidecar_present.set(Some((key, present)));
        present
    }

    /// The card this index reads.
    pub fn card(&self) -> &storage::Mounted {
        self.card
    }

    /// The `audio_id` in the story's TAF header: the version the server
    /// compares in a freshness check.
    ///
    /// Read from the file itself, so a story placed on the card by hand has
    /// one like a downloaded story. `None` when there is no readable story.
    pub fn audio_id(&self, tag: TagUid) -> Option<u32> {
        let path = content_path(tag.0);
        let (file, _size) = self.card.open_content(path.directory, path.file).ok()?;
        // The field is a few dozen bytes in, after the 20-byte hash.
        let mut head = [0u8; 64];
        let read = self.card.read(file, &mut head);
        self.card.close_file(file);
        teddiebox_taf::TonieHeader::audio_id_in(&head[..read.ok()?])
    }

    /// Whether the card ships this story, as opposed to it being downloaded.
    ///
    /// Both kinds are under `CONTENT/`. Stock content is always complete and
    /// has no sidecar; see [`is_stock`].
    ///
    /// Public because whoever handles `Action::Play` must know which kind it
    /// plays, since a download's position is saved apart from stock content.
    pub fn on_stock_card(&self, directory: u32, file: u32) -> bool {
        is_stock(
            self.audio_length(directory, file),
            self.has_sidecar(directory, file),
        )
    }

    /// Records where this story should resume.
    ///
    /// Here rather than in `perform`, because this type already knows a tag's
    /// path and whether it is stock or cached.
    pub fn remember(&self, tag: TagUid, page: u32) -> Result<(), &'static str> {
        let path = content_path(tag.0);
        let stock = self.on_stock_card(path.directory, path.file);
        let mut out = [0u8; MAX_POSITION];
        let len = position::render(page, &mut out);
        self.card
            .write_position(stock, path.directory, path.file, &out[..len])
    }

    fn cached(&self, directory: u32, file: u32) -> Cached {
        let key = (directory, file);
        if let Some((seen, cached)) = &*self.cached.borrow() {
            if *seen == key {
                return cached.clone();
            }
        }
        let cached = self.look_up(directory, file);
        *self.cached.borrow_mut() = Some((key, cached.clone()));
        cached
    }

    fn look_up(&self, directory: u32, file: u32) -> Cached {
        let mut buffer = [0u8; MAX_SIDECAR];
        // A sidecar that is not valid UTF-8 cannot be trusted, and is
        // treated as no sidecar.
        let sidecar = self
            .card
            .read_sidecar(directory, file, &mut buffer)
            .and_then(|len| core::str::from_utf8(&buffer[..len]).ok())
            .and_then(|text| Sidecar::parse(text).ok());

        Cached {
            sidecar,
            length_on_card: self.audio_length(directory, file),
        }
    }
}

impl ContentIndex for CardIndex<'_> {
    fn is_available(&self, tag: TagUid) -> bool {
        let path = content_path(tag.0);

        if self.on_stock_card(path.directory, path.file) {
            return true;
        }

        let cached = self.cached(path.directory, path.file);
        let available = playable_now(false, &cached);

        // A card read error counts as "not here", so the box will try the
        // network even if the story is on the card. The trait cannot return
        // errors, so this is logged instead.
        if !available && cached.length_on_card.is_some() && cached.sidecar.is_none() {
            esp_println::println!(
                "teddiebox: plate /CONTENT/{:08X}/{:08X} has no readable sidecar — refetching",
                path.directory,
                path.file
            );
        }

        available
    }

    /// Whether this figure's story should be checked against the server once
    /// it stops playing.
    ///
    /// Every playable story is checked, a file put on the card by hand as
    /// much as a download: the check compares the `audio_id` in the file's
    /// own header, so the server can replace either, as it does for a
    /// creative-tonie. No check is needed for:
    /// - **An incomplete download**: it goes to the network anyway.
    /// - **A figure already checked since boot**: Wi-Fi uses the most battery,
    ///   and one boot is roughly one play session, since the box switches off
    ///   after five idle minutes.
    fn wants_revalidation(&self, tag: TagUid) -> bool {
        let path = content_path(tag.0);
        if crate::already_asked(tag) {
            return false;
        }
        let stock = self.on_stock_card(path.directory, path.file);
        playable_now(stock, &self.cached(path.directory, path.file))
    }

    /// A probe contradicted this story earlier in the session, while it
    /// played. The download that replaces it clears the mark.
    fn is_stale(&self, tag: TagUid) -> bool {
        let path = content_path(tag.0);
        crate::marked_stale(path.directory, path.file)
    }

    /// Where this story should resume: the position held in RAM if there is
    /// one, otherwise the one saved on the card.
    ///
    /// An unreadable, missing or malformed file gives `Start`; the card
    /// cannot be trusted.
    fn saved_position(&self, tag: TagUid) -> Position {
        // RAM first: it is always newer than the card, which is only written
        // when the RAM copy is about to be lost.
        if let Some(page) = crate::held_place(tag) {
            return Position::Exact { page };
        }
        // Stock: only the last figure resumes, from the one record at the
        // card's root. The per-story files are left alone, not read.
        if !crate::resume_every() {
            return crate::last_played_page(tag)
                .map_or(Position::Start, |page| Position::Exact { page });
        }

        let path = content_path(tag.0);
        let stock = self.on_stock_card(path.directory, path.file);
        let mut buffer = [0u8; MAX_POSITION];
        self.card
            .read_position(stock, path.directory, path.file, &mut buffer)
            .and_then(|len| core::str::from_utf8(&buffer[..len]).ok())
            .map(position::parse)
            .unwrap_or(Position::Start)
    }
}
