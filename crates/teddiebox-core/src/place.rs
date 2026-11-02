//! Where one figure's story got to, kept in RAM until it has to be written to
//! the card.
//!
//! Children lift a figure and put it back again and again. Writing the card on
//! every lift would cost a write each time, so the position is kept in RAM and
//! written to the card only when it would otherwise be lost: when another
//! figure needs the slot, or when the box shuts down.
//!
//! One slot, not a table, matching the stock box's "most recent figure"
//! behaviour. When a second figure replaces the first, the first figure's
//! position is written to the card, so nothing is lost.
//!
//! Trade-off: an unplanned stop (flat battery, crash, console reboot) loses
//! the position held in RAM.

use crate::TagUid;

/// A figure and the page its story reached.
pub type Place = (TagUid, u32);

/// The one place held in RAM, as a write-back cache of a single entry.
#[derive(Debug, Default)]
pub struct PendingPlace {
    slot: Option<Place>,
}

impl PendingPlace {
    pub const fn new() -> Self {
        Self { slot: None }
    }

    /// Stores a figure's position and returns the one it replaced, if any.
    ///
    /// Only a *different* figure replaces anything; the same figure just
    /// updates its page. The caller must write the returned position to the
    /// card, or it is lost.
    pub fn remember(&mut self, tag: TagUid, page: u32) -> Option<Place> {
        let displaced = match self.slot {
            Some((held, held_page)) if held != tag => Some((held, held_page)),
            _ => None,
        };
        self.slot = Some((tag, page));
        displaced
    }

    /// What is held for this figure, if anything.
    ///
    /// Check this before the card: the slot is always newer than the card.
    pub fn held(&self, tag: TagUid) -> Option<u32> {
        match self.slot {
            Some((held, page)) if held == tag => Some(page),
            _ => None,
        }
    }

    /// Forgets the position held for one story, identified by its ruid.
    ///
    /// Keyed by story, not by figure, because the caller (the media task)
    /// knows which file it is playing but not which figure asked for it. A
    /// console `play` of another story that ends must not clear a figure's
    /// position.
    pub fn forget(&mut self, ruid: u64) {
        if matches!(self.slot, Some((held, _)) if held.ruid() == ruid) {
            self.slot = None;
        }
    }

    /// Takes whatever is held, leaving the slot empty.
    ///
    /// Called just before the slot would be lost. A second call returns
    /// nothing, so nothing is written twice.
    pub fn take(&mut self) -> Option<Place> {
        self.slot.take()
    }
}

/// Which stories resume where they stopped, set by `resume` in `config.txt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Resume {
    /// Stock behaviour: only the figure played most recently resumes, also
    /// across power-off. Playing a different figure in between starts the
    /// earlier one over, which is also how a story is restarted on purpose.
    #[default]
    Last,
    /// Every story resumes where it stopped, kept in `<story>.POS` files.
    Every,
}

/// The stock rule for [`Resume::Last`]: which figure, if any, may resume from
/// what the card recorded at the last shutdown.
///
/// The card holds one figure and page (see [`render_last`]). That figure
/// resumes from it only until another figure plays; after that, RAM
/// ([`PendingPlace`]) is the only memory, as on a stock box.
#[derive(Debug, Default)]
pub struct LastPlayed {
    /// What the card said at mount, as `(ruid, page)`.
    on_card: Option<(u64, u32)>,
    /// The figure that played most recently since boot, by ruid.
    played: Option<u64>,
}

impl LastPlayed {
    pub const fn new() -> Self {
        Self {
            on_card: None,
            played: None,
        }
    }

    /// Takes what the card recorded at the last shutdown.
    pub fn loaded(&mut self, on_card: Option<(u64, u32)>) {
        self.on_card = on_card;
    }

    /// A figure starts playing. Returns whether it differs from the one
    /// played before it, in which case the earlier figure's held place must
    /// be dropped, not saved.
    pub fn playing(&mut self, ruid: u64) -> bool {
        let before = self.played.or(self.on_card.map(|(seen, _)| seen));
        self.played = Some(ruid);
        before.is_some_and(|seen| seen != ruid)
    }

    /// The page the card recorded for this figure, if it is still the last
    /// figure played. Check [`PendingPlace::held`] first.
    pub fn from_card(&self, ruid: u64) -> Option<u32> {
        match (self.played, self.on_card) {
            (None, Some((seen, page))) if seen == ruid => Some(page),
            _ => None,
        }
    }
}

/// The longest text [`render_last`] writes: 16 hex digits, a space, a page
/// number of up to 10 digits, and a newline.
pub const MAX_LAST: usize = 28;

/// Writes the last figure played and its page, as `<ruid hex> <page>\n`.
/// Returns how many bytes were written.
pub fn render_last(ruid: u64, page: u32, out: &mut [u8; MAX_LAST]) -> usize {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for (i, slot) in out[..16].iter_mut().enumerate() {
        *slot = HEX[((ruid >> (60 - 4 * i)) & 0xF) as usize];
    }
    out[16] = b' ';
    let mut digits = [0u8; 10];
    let mut n = page;
    let mut count = 0;
    loop {
        digits[count] = b'0' + (n % 10) as u8;
        count += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for i in 0..count {
        out[17 + i] = digits[count - 1 - i];
    }
    out[17 + count] = b'\n';
    18 + count
}

/// Reads what [`render_last`] wrote. Anything else, including an empty file,
/// is `None`: no figure resumes.
pub fn parse_last(text: &str) -> Option<(u64, u32)> {
    let (ruid, page) = text.trim().split_once(' ')?;
    if ruid.len() != 16 {
        return None;
    }
    Some((u64::from_str_radix(ruid, 16).ok()?, page.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real figure, `CONTENT/1D2E3F50/500304E0`.
    const LEO: TagUid = TagUid([0xE0, 0x04, 0x03, 0x50, 0x50, 0x3F, 0x2E, 0x1D]);
    /// Another real figure, *Abends im Walde*, `1E2F4051500304E0`.
    const WALDE: TagUid = TagUid([0xE0, 0x04, 0x03, 0x50, 0x51, 0x40, 0x2F, 0x1E]);

    #[test]
    fn a_fresh_slot_holds_nothing() {
        // Given
        let places = PendingPlace::new();

        // When
        let held = places.held(LEO);

        // Then
        assert_eq!(held, None);
    }

    #[test]
    fn a_remembered_place_is_held_for_that_figure() {
        // Given
        let mut places = PendingPlace::new();

        // When
        places.remember(LEO, 233);

        // Then
        assert_eq!(places.held(LEO), Some(233));
    }

    #[test]
    fn a_place_is_not_held_for_a_different_figure() {
        // Given
        let mut places = PendingPlace::new();

        // When
        places.remember(LEO, 233);

        // Then
        assert_eq!(places.held(WALDE), None);
    }

    /// Lifting a figure and putting it back must not cause a card write.
    #[test]
    fn the_same_figure_again_moves_the_page_on_and_displaces_nothing() {
        // Given
        let mut places = PendingPlace::new();
        places.remember(LEO, 233);

        // When
        let displaced = places.remember(LEO, 247);

        // Then
        assert_eq!(displaced, None);
        assert_eq!(places.held(LEO), Some(247));
    }

    /// The caller writes the replaced position to the card.
    #[test]
    fn a_different_figure_displaces_the_one_held_and_hands_it_back() {
        // Given
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);

        // When
        let displaced = places.remember(WALDE, 12);

        // Then
        assert_eq!(displaced, Some((LEO, 284)));
    }

    #[test]
    fn a_displaced_figure_is_no_longer_held() {
        // Given
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);

        // When
        places.remember(WALDE, 12);

        // Then
        assert_eq!(places.held(LEO), None);
        assert_eq!(places.held(WALDE), Some(12));
    }

    /// A finished story has no position to keep. The slot must be cleared
    /// too, because it is checked before the card.
    #[test]
    fn a_story_that_ended_is_forgotten() {
        // Given
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);

        // When
        places.forget(LEO.ruid());

        // Then
        assert_eq!(places.held(LEO), None);
    }

    /// Keyed by story, not by figure: a console `play` that ends must not
    /// clear a figure's position.
    #[test]
    fn a_different_story_ending_leaves_the_slot_alone() {
        // Given
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);

        // When
        places.forget(WALDE.ruid());

        // Then
        assert_eq!(places.held(LEO), Some(284));
    }

    #[test]
    fn taking_the_place_hands_it_back_and_empties_the_slot() {
        // Given
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);

        // When
        let taken = places.take();

        // Then
        assert_eq!(taken, Some((LEO, 284)));
        assert_eq!(places.held(LEO), None);
    }

    /// A second call at shutdown must not write the same position again.
    #[test]
    fn a_second_take_hands_back_nothing() {
        // Given
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);
        assert_eq!(places.take(), Some((LEO, 284)));

        // When
        let taken = places.take();

        // Then
        assert_eq!(taken, None);
    }

    /// After power-off, the figure recorded at shutdown resumes.
    #[test]
    fn the_figure_recorded_at_shutdown_resumes_after_boot() {
        // Given
        let mut last = LastPlayed::new();

        // When
        last.loaded(Some((LEO.ruid(), 284)));

        // Then
        assert_eq!(last.from_card(LEO.ruid()), Some(284));
        assert_eq!(last.from_card(WALDE.ruid()), None);
    }

    /// Stock: playing another figure first starts the recorded one over.
    #[test]
    fn another_figure_played_first_starts_the_recorded_one_over() {
        // Given
        let mut last = LastPlayed::new();
        last.loaded(Some((LEO.ruid(), 284)));

        // When
        let other = last.playing(WALDE.ruid());

        // Then
        assert!(other);
        assert_eq!(last.from_card(LEO.ruid()), None);
    }

    /// Placing B briefly and then A again restarts A: the caller drops A's
    /// held place when B plays.
    #[test]
    fn a_different_figure_playing_says_to_drop_the_earlier_place() {
        // Given
        let mut last = LastPlayed::new();
        last.playing(LEO.ruid());

        // When
        let swapped = last.playing(WALDE.ruid());
        let back = last.playing(LEO.ruid());

        // Then
        assert!(swapped);
        assert!(back);
    }

    /// Lifting a figure and putting it back keeps its place.
    #[test]
    fn the_same_figure_again_keeps_its_place() {
        // Given
        let mut last = LastPlayed::new();
        last.playing(LEO.ruid());

        // When
        let again = last.playing(LEO.ruid());

        // Then
        assert!(!again);
    }

    /// The first figure after boot, when nothing was recorded, has nothing
    /// to drop.
    #[test]
    fn the_first_figure_after_an_empty_record_drops_nothing() {
        // Given
        let mut last = LastPlayed::new();
        last.loaded(None);

        // When
        let other = last.playing(LEO.ruid());

        // Then
        assert!(!other);
    }

    #[test]
    fn the_record_reads_back_what_was_written() {
        for (ruid, page) in [(LEO.ruid(), 0), (WALDE.ruid(), 284), (u64::MAX, u32::MAX)] {
            // Given
            let mut out = [0u8; MAX_LAST];

            // When
            let len = render_last(ruid, page, &mut out);
            let text = core::str::from_utf8(&out[..len]).unwrap();

            // Then
            assert_eq!(parse_last(text), Some((ruid, page)), "{text:?}");
        }
    }

    #[test]
    fn an_empty_or_damaged_record_resumes_nothing() {
        for text in [
            "",
            "\n",
            "E00403",
            "1D2E3F50500304E0",
            "1D2E3F50500304E0 x",
            "zz 1",
        ] {
            assert_eq!(parse_last(text), None, "{text:?}");
        }
    }
}
