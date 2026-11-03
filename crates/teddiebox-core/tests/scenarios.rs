//! Whole-session walkthroughs. These read as a description of using the box.

use teddiebox_core::*;

const TAG: TagUid = TagUid([1, 2, 3, 4, 5, 6, 7, 8]);

struct Library {
    available: bool,
    resume: Position,
    /// Cached, and not yet asked about since the box booted.
    unchecked: bool,
    /// A check this session found the server has a newer story.
    stale: bool,
}

impl ContentIndex for Library {
    fn is_available(&self, _tag: TagUid) -> bool {
        self.available
    }
    fn saved_position(&self, _tag: TagUid) -> Position {
        self.resume
    }
    fn wants_revalidation(&self, _tag: TagUid) -> bool {
        self.unchecked
    }
    fn is_stale(&self, _tag: TagUid) -> bool {
        self.stale
    }
}

fn has(actions: &Actions, wanted: Action) -> bool {
    actions.contains(&wanted)
}

#[test]
fn a_child_plays_a_story_adjusts_the_volume_and_lifts_the_figure() {
    // Given
    let mut core = Core::new(CoreConfig::default());
    let library = Library {
        available: true,
        resume: Position::Exact { page: 1 },
        unchecked: false,
        stale: false,
    };

    // When: the figure goes on, an ear is tapped, the box is slapped, and the
    // figure comes off
    let start = core.handle(Event::TagPresent(TAG), &library);
    core.handle(Event::EarDown(Ear::Larger, 1_000), &library);
    let louder = core.handle(Event::EarUp(Ear::Larger, 1_100), &library);
    let slap = core.handle(Event::Slap(Side::Right), &library);
    let lift = core.handle(Event::TagAbsent, &library);

    // Then
    assert!(has(
        &start,
        Action::Play {
            tag: TAG,
            from: Position::Exact { page: 1 }
        }
    ));
    assert!(louder.iter().any(|a| matches!(a, Action::SetVolume { .. })));
    assert!(has(&slap, Action::NextTrack));
    assert!(lift
        .iter()
        .any(|a| matches!(a, Action::SavePosition { .. })));
    assert!(has(&lift, Action::Pause));
}

#[test]
fn an_unknown_figure_is_fetched_then_played() {
    // Given
    let mut core = Core::new(CoreConfig::default());
    let empty = Library {
        available: false,
        resume: Position::default(),
        unchecked: false,
        stale: false,
    };
    let downloaded = Library {
        available: true,
        resume: Position::Exact { page: 1 },
        unchecked: false,
        stale: false,
    };

    // When: the figure goes on an empty card, and its download finishes
    let placed = core.handle(Event::TagPresent(TAG), &empty);
    let ready = core.handle(Event::ContentReady(TAG), &downloaded);

    // Then
    assert!(has(&placed, Action::RequestContent(TAG)));
    assert!(has(&placed, Action::SetLed(LedState::Fetching)));
    assert!(has(
        &ready,
        Action::Play {
            tag: TAG,
            from: Position::Exact { page: 1 }
        }
    ));
}

#[test]
fn an_exhausted_pack_powers_the_box_off_mid_story() {
    // Given: a story playing
    let mut core = Core::new(CoreConfig::default());
    let library = Library {
        available: true,
        resume: Position::Exact { page: 1 },
        unchecked: false,
        stale: false,
    };
    core.handle(Event::TagPresent(TAG), &library);

    // When: flat readings, as many as it takes for them to agree
    let flat = Event::Battery {
        pack_mv: 2_900,
        under_load: false,
    };
    let readings = [(); 4].map(|_| core.handle(flat, &library));

    // Then
    assert!(has(
        &readings[3],
        Action::PowerOff(PowerOffReason::PackEmpty)
    ));
}

/// A figure placed for the first time since boot, whose story is already on
/// the card. The story plays at once while the box asks the server, which
/// says the file has changed. The next time the figure goes on, the new story
/// is downloaded and starts where the child left off.
#[test]
fn a_cached_story_the_server_has_changed_is_fetched_on_the_next_placement() {
    // Given
    let mut core = Core::new(CoreConfig::default());
    let mut cached = Library {
        available: true,
        resume: Position::Exact { page: 412 },
        unchecked: true,
        stale: false,
    };

    // When: the figure goes on and comes off, the server says the story has
    // changed, the figure goes back on, and the download finishes
    let placed = core.handle(Event::TagPresent(TAG), &cached);
    let lifted = core.handle(Event::TagAbsent, &cached);
    let stale = core.handle(Event::Revalidated(TAG, Freshness::Stale), &cached);
    cached.unchecked = false;
    cached.stale = true;
    let replaced = core.handle(Event::TagPresent(TAG), &cached);
    let ready = core.handle(Event::ContentReady(TAG), &cached);

    // Then: the old story played without waiting, and the new one replaced it
    // on the next placement
    let play = Action::Play {
        tag: TAG,
        from: Position::Exact { page: 412 },
    };
    assert!(has(&placed, play));
    assert!(!has(&placed, Action::Revalidate(TAG)));
    assert!(has(&lifted, Action::Revalidate(TAG)));
    assert!(
        !stale
            .iter()
            .any(|a| matches!(a, Action::Pause | Action::Stop)),
        "the answer does not interrupt the story: {stale:?}"
    );
    assert!(has(&replaced, Action::RequestContent(TAG)));
    assert!(has(&ready, play));
}

/// Offline: the server cannot be reached. The cached story was already
/// playing, and keeps playing.
#[test]
fn a_story_the_server_could_not_be_asked_about_still_plays() {
    // Given
    let mut core = Core::new(CoreConfig::default());
    let cached = Library {
        available: true,
        resume: Position::Start,
        unchecked: true,
        stale: false,
    };

    // When
    let placed = core.handle(Event::TagPresent(TAG), &cached);
    let answered = core.handle(Event::Revalidated(TAG, Freshness::Current), &cached);

    // Then
    assert!(has(
        &placed,
        Action::Play {
            tag: TAG,
            from: Position::Start
        }
    ));
    assert!(
        !answered
            .iter()
            .any(|a| matches!(a, Action::Pause | Action::Stop)),
        "{answered:?}"
    );
}
