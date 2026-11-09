//! What the box is doing, and what placing or lifting a figure changes.

use crate::cue::Cue;
use crate::{Action, Actions, PlaybackKind, Position, Prompt, TagUid};

/// Answers whether content is available locally, and where playback last
/// stopped. Implemented by the firmware over the SD card; stubbed in tests.
pub trait ContentIndex {
    fn is_available(&self, tag: TagUid) -> bool;
    fn saved_position(&self, tag: TagUid) -> Position;
    /// Whether this figure's story should be checked against the server
    /// before it plays.
    ///
    /// Separate from [`Self::is_available`]: that asks whether there is
    /// anything to play, this asks whether it needs checking. Only downloaded
    /// content can need checking; a file placed under `CONTENT/` by hand has
    /// no sidecar to compare against.
    fn wants_revalidation(&self, tag: TagUid) -> bool;
    /// Whether a check this session found that the server has a different
    /// story for this figure. The next placement downloads it instead of
    /// playing the old copy.
    fn is_stale(&self, tag: TagUid) -> bool;
    /// Whether the card holds the start of this story, enough to play while
    /// the rest is fetched, without holding all of it. Only such a story is
    /// played when the network cannot be reached.
    fn has_playable_prefix(&self, _tag: TagUid) -> bool {
        false
    }
}

/// What the server said about a story already on the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Nothing contradicted the cached copy, including when the server could
    /// not be reached. Being offline is not a reason to refuse a story.
    Current,
    /// The server has a different file under this figure's name.
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Fetching(TagUid),
    Playing(TagUid),
    Failed,
}

/// Why a figure's story could not be produced.
///
/// Split by what the user can do about it: a network problem they can check,
/// or a figure the server has no story for, which they cannot fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// Association, DHCP, TLS, the socket, a timeout, or a server that
    /// answered with a fault of its own.
    Unreachable,
    /// The access point was there and turned the box away: the passphrase on
    /// the card is not the one it wants.
    ///
    /// Use this only when sure. Anything uncertain belongs in
    /// [`Unavailable::Unreachable`]: telling somebody their passphrase is
    /// wrong when the router was just off makes them retype a correct one.
    Refused,
    /// The server was reached and has nothing for this figure.
    NoContent,
}

#[derive(Debug)]
pub struct Playback {
    state: State,
    position: Position,
    /// The playing figure, if its story still needs checking with the
    /// server. Asked once nothing plays: on one core, bringing Wi-Fi up
    /// stalls the audio.
    unchecked: Option<TagUid>,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            state: State::Idle,
            position: Position::default(),
            unchecked: None,
        }
    }
}

impl Playback {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn kind(&self) -> PlaybackKind {
        match self.state {
            State::Idle => PlaybackKind::Idle,
            State::Fetching(_) => PlaybackKind::Fetching,
            State::Playing(_) => PlaybackKind::Playing,
            State::Failed => PlaybackKind::Failed,
        }
    }

    /// Records how far playback has advanced, so lifting the figure can save it.
    pub fn note_position(&mut self, pos: Position) {
        self.position = pos;
    }

    pub fn on_tag_present<I: ContentIndex>(&mut self, tag: TagUid, index: &I) -> Actions {
        let mut actions = Actions::new();
        // Already handling this figure, so ignore the repeat. Acting on it is
        // harmful: while a story is playing its file is open, and
        // `embedded-sdmmc` cannot open a file twice, so the card would report
        // the story missing and the box would download it again.
        if matches!(
            self.state,
            State::Playing(seen) | State::Fetching(seen) if seen == tag
        ) {
            return actions;
        }
        if index.is_available(tag) && !index.is_stale(tag) {
            let from = index.saved_position(tag);
            self.position = from;
            self.state = State::Playing(tag);
            let _ = actions.push(Action::Play { tag, from });
            // Play first, ask once it stops. A newer story replaces this one
            // on the next placement: replacing a file while it plays would
            // need two handles on one file and a rename, and `embedded-sdmmc`
            // supports neither.
            self.unchecked = index.wants_revalidation(tag).then_some(tag);
        } else {
            self.state = State::Fetching(tag);
            let _ = actions.push(Action::RequestContent(tag));
        }
        actions
    }

    pub fn on_tag_absent(&mut self) -> Actions {
        let mut actions = Actions::new();
        match self.state {
            State::Playing(tag) => {
                let _ = actions.push(Action::SavePosition {
                    tag,
                    pos: self.position,
                });
                let _ = actions.push(Action::Pause);
                self.ask_if_unchecked(tag, &mut actions);
            }
            // The download goes on without the figure, so the story is there,
            // or further along, when it is placed again. Only a different
            // story's request stops it; the firmware decides that.
            State::Fetching(_) | State::Idle | State::Failed => {}
        }
        self.state = State::Idle;
        actions
    }

    /// What the server said about a cached story.
    ///
    /// The story has already played by the time this arrives, so nothing
    /// changes now. A stale story is marked by the firmware, and
    /// [`ContentIndex::is_stale`] sends the next placement to the network.
    pub fn on_revalidated(&mut self, _tag: TagUid, _freshness: Freshness) -> Actions {
        Actions::new()
    }

    pub fn on_content_ready<I: ContentIndex>(&mut self, tag: TagUid, index: &I) -> Actions {
        let mut actions = Actions::new();
        // The story is already playing from its own download, which has now
        // finished: say so over it, as a stock box does.
        if self.state == State::Playing(tag) {
            let _ = actions.push(Action::PlayCue(Cue::Downloaded));
            return actions;
        }
        // Ignore a download that finished after the figure was lifted or swapped.
        if self.state != State::Fetching(tag) {
            return actions;
        }
        let from = index.saved_position(tag);
        self.position = from;
        self.state = State::Playing(tag);
        let _ = actions.push(Action::Play { tag, from });
        actions
    }

    /// The story reached its end on its own.
    ///
    /// Unlike a lift, nothing is saved: a finished story is not paused, and
    /// its saved position has just been cleared. Unlike `TrackFinished`, this
    /// does not move to the next chapter.
    ///
    /// Only a playing story can end. A download in progress is left alone,
    /// because the sound that finished was something else.
    pub fn on_playback_ended(&mut self) -> Actions {
        let mut actions = Actions::new();
        if let State::Playing(tag) = self.state {
            self.state = State::Idle;
            self.ask_if_unchecked(tag, &mut actions);
        }
        actions
    }

    /// Asks the server about a story that just stopped playing, if it still
    /// needs asking.
    fn ask_if_unchecked(&mut self, tag: TagUid, actions: &mut Actions) {
        if self.unchecked.take() == Some(tag) {
            let _ = actions.push(Action::Revalidate(tag));
        }
    }

    pub fn on_content_missing<I: ContentIndex>(
        &mut self,
        tag: TagUid,
        why: Unavailable,
        index: &I,
    ) -> Actions {
        let mut actions = Actions::new();
        if self.state != State::Fetching(tag) {
            return actions;
        }
        // The network is not there, but the card has the start of the
        // story: play that, as if it were still downloading. The firmware
        // keeps trying the network meanwhile.
        if why != Unavailable::NoContent && index.has_playable_prefix(tag) {
            let from = index.saved_position(tag);
            self.position = from;
            self.state = State::Playing(tag);
            let _ = actions.push(Action::Play { tag, from });
            return actions;
        }
        self.state = State::Failed;
        let _ = actions.push(Action::PlayPrompt(match why {
            Unavailable::Unreachable => Prompt::NoNetwork,
            Unavailable::Refused => Prompt::WrongPassword,
            Unavailable::NoContent => Prompt::NoContent,
        }));
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use heapless::Vec;

    const TAG: TagUid = TagUid([1, 2, 3, 4, 5, 6, 7, 8]);
    const OTHER: TagUid = TagUid([9, 9, 9, 9, 9, 9, 9, 9]);

    struct Index {
        available: bool,
        resume: Position,
        wants_asking: bool,
        stale: bool,
    }

    impl ContentIndex for Index {
        fn is_available(&self, _tag: TagUid) -> bool {
            self.available
        }
        fn saved_position(&self, _tag: TagUid) -> Position {
            self.resume
        }
        fn wants_revalidation(&self, _tag: TagUid) -> bool {
            self.wants_asking
        }
        fn is_stale(&self, _tag: TagUid) -> bool {
            self.stale
        }
    }

    fn known(page: u32) -> Index {
        Index {
            available: true,
            resume: Position::Exact { page },
            wants_asking: false,
            stale: false,
        }
    }

    fn unknown() -> Index {
        Index {
            available: false,
            resume: Position::default(),
            wants_asking: false,
            stale: false,
        }
    }

    /// Cached, complete, and not yet asked about since the box booted.
    fn unchecked(page: u32) -> Index {
        Index {
            available: true,
            resume: Position::Exact { page },
            wants_asking: true,
            stale: false,
        }
    }

    /// A cached story that may be out of date plays at once. Nothing is
    /// asked while it plays: on one core, Wi-Fi coming up stalls the audio.
    #[test]
    fn a_figure_that_has_not_been_asked_about_plays_without_asking() {
        // Given
        let mut p = Playback::new();

        // When
        let actions = p.on_tag_present(TAG, &unchecked(412));

        // Then
        assert_eq!(
            actions.as_slice(),
            &[Action::Play {
                tag: TAG,
                from: Position::Exact { page: 412 }
            }]
        );
        assert_eq!(p.kind(), PlaybackKind::Playing);
    }

    /// Lifting the figure is when the server is asked.
    #[test]
    fn lifting_an_unchecked_figure_asks_about_it() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unchecked(412));
        p.note_position(Position::Exact { page: 500 });

        // When
        let actions = p.on_tag_absent();

        // Then
        assert_eq!(
            actions.as_slice(),
            &[
                Action::SavePosition {
                    tag: TAG,
                    pos: Position::Exact { page: 500 }
                },
                Action::Pause,
                Action::Revalidate(TAG)
            ]
        );
    }

    /// A story that plays to its end is asked about then.
    #[test]
    fn an_unchecked_story_that_ends_is_asked_about() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unchecked(1));

        // When
        let actions = p.on_playback_ended();

        // Then
        assert_eq!(actions.as_slice(), &[Action::Revalidate(TAG)]);
    }

    /// Asked once: a second lift does not ask again.
    #[test]
    fn a_figure_is_asked_about_once_per_placement() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unchecked(1));
        p.on_tag_absent();
        p.on_tag_present(TAG, &known(1));

        // When
        let actions = p.on_tag_absent();

        // Then
        assert!(!actions.contains(&Action::Revalidate(TAG)), "{actions:?}");
    }

    /// The answer changes nothing at once; a stale story is replaced on the
    /// next placement.
    #[test]
    fn an_answer_changes_nothing_at_once() {
        for freshness in [Freshness::Current, Freshness::Stale] {
            // Given
            let mut p = Playback::new();
            p.on_tag_present(TAG, &unchecked(412));
            p.on_tag_absent();

            // When
            let actions = p.on_revalidated(TAG, freshness);

            // Then
            assert!(actions.as_slice().is_empty(), "{freshness:?}");
        }
    }

    /// A story the server has changed is downloaded again on the next
    /// placement, the same way as a figure with no story.
    #[test]
    fn a_stale_story_is_fetched_on_the_next_placement() {
        // Given
        let mut p = Playback::new();
        let stale = Index {
            available: true,
            resume: Position::Exact { page: 412 },
            wants_asking: false,
            stale: true,
        };

        // When
        let actions = p.on_tag_present(TAG, &stale);

        // Then
        assert_eq!(actions.as_slice(), &[Action::RequestContent(TAG)]);
        assert_eq!(p.kind(), PlaybackKind::Fetching);
    }

    /// A figure already checked since boot plays without asking the server
    /// again: Wi-Fi uses the most battery.
    #[test]
    fn a_figure_already_asked_about_plays_straight_away() {
        // Given
        let mut p = Playback::new();

        // When
        let actions = p.on_tag_present(TAG, &known(3));

        // Then
        assert_eq!(
            actions.as_slice(),
            &[Action::Play {
                tag: TAG,
                from: Position::Exact { page: 3 }
            }]
        );
    }

    /// A repeated arrival of the playing figure must do nothing. While the
    /// story plays its file is open, and `embedded-sdmmc` cannot open a file
    /// twice, so asking the card would report the story missing and start a
    /// needless download.
    #[test]
    fn a_figure_already_playing_is_not_started_again() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &known(1));

        // When
        let again = p.on_tag_present(TAG, &unknown());

        // Then
        assert!(
            again.as_slice().is_empty(),
            "a repeat arrival must do nothing at all: {again:?}"
        );
        assert_eq!(p.kind(), PlaybackKind::Playing);
    }

    /// The same for a download in progress: a repeated arrival must not
    /// restart it from zero.
    #[test]
    fn a_figure_already_being_fetched_is_not_fetched_again() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());

        // When
        let again = p.on_tag_present(TAG, &unknown());

        // Then
        assert!(again.as_slice().is_empty(), "got: {again:?}");
        assert_eq!(p.kind(), PlaybackKind::Fetching);
    }

    /// A *different* figure is a real change and must still be acted on.
    #[test]
    fn a_different_figure_still_replaces_the_one_playing() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &known(1));

        // When
        let swapped = p.on_tag_present(OTHER, &known(7));

        // Then
        assert_eq!(
            swapped.as_slice(),
            &[Action::Play {
                tag: OTHER,
                from: Position::Exact { page: 7 }
            }]
        );
    }

    #[test]
    fn placing_a_known_figure_plays_from_the_start() {
        // Given: a figure on the card with nothing saved for it
        let mut p = Playback::new();
        let never_played = Index {
            available: true,
            resume: Position::Start,
            wants_asking: false,
            stale: false,
        };

        // When
        let actions = p.on_tag_present(TAG, &never_played);

        // Then
        assert_eq!(
            actions.as_slice(),
            &[Action::Play {
                tag: TAG,
                from: Position::Start
            }]
        );
        assert_eq!(p.kind(), PlaybackKind::Playing);
    }

    #[test]
    fn placing_an_unknown_figure_requests_the_content() {
        // Given
        let mut p = Playback::new();

        // When
        let actions = p.on_tag_present(TAG, &unknown());

        // Then
        assert_eq!(actions.as_slice(), &[Action::RequestContent(TAG)]);
        assert_eq!(p.kind(), PlaybackKind::Fetching);
    }

    #[test]
    fn lifting_the_figure_saves_the_position_and_pauses() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &known(1));
        p.note_position(Position::Exact { page: 77 });

        // When
        let actions: Vec<Action, 8> = p.on_tag_absent();

        // Then
        assert_eq!(
            actions.as_slice(),
            &[
                Action::SavePosition {
                    tag: TAG,
                    pos: Position::Exact { page: 77 }
                },
                Action::Pause,
            ]
        );
        assert_eq!(p.kind(), PlaybackKind::Idle);
    }

    #[test]
    fn replacing_the_same_figure_resumes_from_the_saved_position() {
        // Given: played to page 77, then lifted
        let mut p = Playback::new();
        p.on_tag_present(TAG, &known(1));
        p.note_position(Position::Exact { page: 77 });
        p.on_tag_absent();

        // When
        let actions = p.on_tag_present(TAG, &known(77));

        // Then
        assert_eq!(
            actions.as_slice(),
            &[Action::Play {
                tag: TAG,
                from: Position::Exact { page: 77 }
            }]
        );
    }

    #[test]
    fn a_completed_download_starts_playback() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());

        // When
        let actions = p.on_content_ready(TAG, &known(1));

        // Then
        assert_eq!(
            actions.as_slice(),
            &[Action::Play {
                tag: TAG,
                from: Position::Exact { page: 1 }
            }]
        );
        assert_eq!(p.kind(), PlaybackKind::Playing);
    }

    #[test]
    fn a_download_completing_for_a_figure_no_longer_present_is_ignored() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());

        // When
        let actions = p.on_content_ready(OTHER, &known(1));

        // Then
        assert!(actions.is_empty());
        assert_eq!(p.kind(), PlaybackKind::Fetching);
    }

    #[test]
    fn a_figure_the_server_has_no_story_for_says_so() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());

        // When
        let actions = p.on_content_missing(TAG, Unavailable::NoContent, &unknown());

        // Then
        assert_eq!(actions.as_slice(), &[Action::PlayPrompt(Prompt::NoContent)]);
        assert_eq!(p.kind(), PlaybackKind::Failed);
    }

    /// A network problem gets its own prompt, because the user can check it.
    #[test]
    fn a_figure_that_could_not_be_reached_blames_the_network() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());

        // When
        let actions = p.on_content_missing(TAG, Unavailable::Unreachable, &unknown());

        // Then
        assert_eq!(actions.as_slice(), &[Action::PlayPrompt(Prompt::NoNetwork)]);
        assert_eq!(p.kind(), PlaybackKind::Failed);
    }

    /// A refused passphrase is not a missing network. The user should check
    /// the passphrase on the card, not the router.
    #[test]
    fn a_figure_whose_network_refused_the_passphrase_blames_the_passphrase() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());

        // When
        let actions = p.on_content_missing(TAG, Unavailable::Refused, &unknown());

        // Then
        assert_eq!(
            actions.as_slice(),
            &[Action::PlayPrompt(Prompt::WrongPassword)]
        );
        assert_eq!(p.kind(), PlaybackKind::Failed);
    }

    #[test]
    fn a_failure_for_a_figure_already_lifted_is_ignored() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());

        // When
        let actions = p.on_content_missing(OTHER, Unavailable::NoContent, &unknown());

        // Then
        assert!(actions.is_empty());
    }

    /// The network cannot be reached, but the card holds the first minutes
    /// of the story: they play, and no prompt sounds.
    #[test]
    fn an_unreachable_server_plays_the_partial_the_card_holds() {
        // Given
        struct Partial;
        impl ContentIndex for Partial {
            fn is_available(&self, _tag: TagUid) -> bool {
                false
            }
            fn saved_position(&self, _tag: TagUid) -> Position {
                Position::Exact { page: 40 }
            }
            fn wants_revalidation(&self, _tag: TagUid) -> bool {
                false
            }
            fn is_stale(&self, _tag: TagUid) -> bool {
                false
            }
            fn has_playable_prefix(&self, _tag: TagUid) -> bool {
                true
            }
        }
        let mut p = Playback::new();
        p.on_tag_present(TAG, &Partial);
        assert_eq!(p.kind(), PlaybackKind::Fetching);

        // When
        let actions = p.on_content_missing(TAG, Unavailable::Unreachable, &Partial);

        // Then
        assert_eq!(
            actions.as_slice(),
            &[Action::Play {
                tag: TAG,
                from: Position::Exact { page: 40 }
            }]
        );
        assert_eq!(p.kind(), PlaybackKind::Playing);
    }

    /// A story playing while it downloads: when the download finishes, a
    /// chime over the story says so, and nothing else changes.
    #[test]
    fn a_download_finishing_under_its_own_playing_story_chimes() {
        // Given
        let mut p = Playback::new();
        let playing = Index {
            available: true,
            resume: Position::Start,
            wants_asking: false,
            stale: false,
        };
        p.on_tag_present(TAG, &playing);
        assert_eq!(p.kind(), PlaybackKind::Playing);

        // When
        let actions = p.on_content_ready(TAG, &playing);

        // Then
        assert_eq!(actions.as_slice(), &[Action::PlayCue(Cue::Downloaded)]);
        assert_eq!(p.kind(), PlaybackKind::Playing);
    }

    /// Lifting a figure mid-download leaves the download running, so the
    /// story is further along, or complete, when the figure comes back.
    #[test]
    fn lifting_a_figure_that_is_still_fetching_leaves_the_download_running() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());
        assert_eq!(p.kind(), PlaybackKind::Fetching);

        // When
        let actions = p.on_tag_absent();

        // Then
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(p.kind(), PlaybackKind::Idle);
    }

    #[test]
    fn lifting_a_playing_figure_does_not_abort_anything() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &known(7));

        // When
        let actions = p.on_tag_absent();

        // Then
        assert_eq!(
            actions.as_slice(),
            &[
                Action::SavePosition {
                    tag: TAG,
                    pos: Position::Exact { page: 7 }
                },
                Action::Pause
            ]
        );
    }

    #[test]
    fn lifting_from_an_empty_plate_does_nothing() {
        // Given
        let mut p = Playback::new();

        // When
        let actions = p.on_tag_absent();

        // Then
        assert!(actions.is_empty());
    }

    /// A story that reaches its end leaves the box idle, with the figure still
    /// on the box, so the LED updates and the idle timeout can fire.
    #[test]
    fn a_story_reaching_its_end_leaves_the_box_idle() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &known(1));

        // When
        let actions = p.on_playback_ended();

        // Then
        assert!(actions.is_empty());
        assert_eq!(p.kind(), PlaybackKind::Idle);
    }

    /// No `SavePosition`: a finished story is not paused, and its position was
    /// cleared on purpose. No `Pause` either: nothing is playing.
    #[test]
    fn a_story_reaching_its_end_saves_no_position() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &known(1));
        p.note_position(Position::Exact { page: 900 });

        // When
        let actions = p.on_playback_ended();

        // Then
        assert!(actions.is_empty());
    }

    /// A story ending while the box was fetching something else, or idle already,
    /// changes nothing.
    #[test]
    fn an_end_with_nothing_playing_is_ignored() {
        // Given
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());

        // When
        let actions = p.on_playback_ended();

        // Then
        assert!(actions.is_empty());
        assert_eq!(
            p.kind(),
            PlaybackKind::Fetching,
            "a fetch is not interrupted"
        );
    }
}
