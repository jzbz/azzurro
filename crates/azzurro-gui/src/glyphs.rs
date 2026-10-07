//! Choosing an icon for a browse row.
//!
//! The player supplies a picture for almost every row, but they are not all
//! the same kind of thing. A cover, a station logo and a service's own brand
//! mark are content: the player is telling us something we could not work out
//! ourselves, and replacing them would be vandalism. A PNG of a television for
//! the HDMI input, or of a stack of paper for "Playlists", is interface
//! furniture — drawn in BluOS's style, which beside a Lucide set reads as
//! somebody else's icons pasted in.
//!
//! So furniture is replaced and content is kept. The two are told apart by
//! where the player keeps them, which is consistent across every document
//! captured from a real player: service artwork lives under `/Sources/images/`
//! or `/images/ui/Source/`, and the rest of `/images/` is chrome.
//!
//! The same split runs through the words. The vocabulary in [`reading`] is the
//! player's names for its own menus, inputs and verbs — "Shuffle", "Add to
//! playlist…", "Library", "HDMI ARC" — and it is only ever right about rows
//! that are those things. A row the player presents as a song, an album, an
//! artist or a station has a title somebody else wrote, and reading that
//! against the same words drew one track of an album, "Barometer Song", with
//! the track glyph in a column of tracks drawn with the plain note.
//! [`row_glyph`] tells the two kinds of row apart by what the player says
//! about the row, never by what the row is called.

use bluos::screen::{Action, Item, SectionKind};

/// A Lucide glyph to draw instead of the player's own picture.
///
/// `Hash` so it can go into the memo fingerprints: a glyph is drawn, and a
/// field that is drawn but not hashed is a cell that stops redrawing when it
/// changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Glyph {
    Play,
    Bluetooth,
    Tv,
    Cable,
    Usb,
    Playlist,
    Library,
    Radio,
    /// Radio Paradise, whose own brand mark is a pair of headphones. Its own
    /// glyph rather than the general radio one because it sits in the sidebar
    /// directly under Radio, and two services one above the other under the
    /// same picture read as one entry drawn twice.
    Headphones,
    /// TuneIn: a dot between two arcs, which is a signal going out rather than
    /// a set receiving one. The three radio services in the sidebar are told
    /// apart by their pictures, so each keeps one of its own.
    Broadcast,
    Station,
    Favourite,
    Preset,
    Search,
    Album,
    Artist,
    Track,
    Genre,
    Folder,
    Recent,
    Add,
    Home,
    News,
    Sources,
    Shuffle,
    Info,
    Details,
    Enqueue,
    Unfavourite,
    Clear,
    Save,
    Settings,
    /// The generic stand-in for a setting with no icon of its own.
    Tweak,
    // The settings vocabulary. These names come off the player's own pages —
    // "Amplifier Standby", "Indicator brightness", "Reindex music collection"
    // — and without them every row on every settings page draws the same
    // generic slider, which tells the reader nothing.
    Alarm,
    Sleep,
    Speaker,
    Volume,
    Wifi,
    Network,
    Artwork,
    Power,
    Brightness,
    Reset,
    Server,
    Tone,
    Gauge,
    Edit,
    /// A field whose value is never drawn back.
    Secret,
    // A radio service writes its own menu, and the words are always much the
    // same: somewhere to start, somewhere near you, and the usual shelves.
    ForYou,
    Sport,
    Podcast,
    Trending,
    Language,
    Local,
    Place,
    /// A music service with no glyph of its own — the long tail of them, from
    /// Deezer to whatever the next firmware adds.
    Service,
    Rescan,
}

/// Whether `source` is something worth showing as it is.
///
/// Absolute URLs are a service's CDN, `/Artwork` is the player's own art
/// endpoint, and the two service-icon directories hold brand marks.
fn is_content(source: &str) -> bool {
    // `/Sources/images/` and `/images/ui/Source/` are deliberately not here.
    // Those two hold the services' own brand marks — the Radio Paradise
    // headphones, the TuneIn wordmark — and a brand mark is not content: it
    // says which service a row belongs to, which the row's own name already
    // says. Beside a column of Lucide strokes they read as somebody else's
    // icons pasted in, so the glyph wins.
    //
    // What stays is the artwork that carries information nothing else does: a
    // cover, a station's own picture, anything off a service's CDN.
    source.starts_with("http") || source.contains("/Artwork")
}

/// The glyph for a row in a service's own menu.
///
/// Unlike [`glyph_for`], this never defers to the player's picture and always
/// returns something. A menu row's image is the service's branding for a
/// category — TuneIn ships a colored logo for "Sports" and another for
/// "Trending" — and beside a column of Lucide strokes those read as somebody
/// else's icons pasted in. The rows that carry real artwork are the ones that
/// play something, and they go through [`row_glyph`], which reads a thing by
/// its picture and never by its title.
///
/// The fallback is a station, because a category in a radio service leads to
/// stations however it is named — and some of them, like the listener's own
/// country, cannot be recognized from the word alone.
pub fn menu_glyph(title: &str) -> Glyph {
    glyph_for(title, None).unwrap_or(Glyph::Station)
}

/// The glyph for a music service, whatever it is called.
///
/// Unlike [`glyph_for`], this never defers to the player's own picture. The
/// sidebar draws the services as a list of names, and the brand marks beside
/// them are PNGs of somebody else's logo; in a column of Lucide strokes they
/// read as a rip in the page. Names it recognizes get something apt, and the
/// rest get a note.
pub fn service_glyph(title: &str) -> Glyph {
    glyph_for(title, None).unwrap_or(Glyph::Service)
}

/// The glyph to draw for a row, or `None` to use whatever the player sent.
///
/// The title is matched before the path, because the title is what the reader
/// is looking at: a row called "Albums" should get the album glyph whichever
/// PNG happens to sit beside it.
///
/// Meant for a row whose title is the player's own word: a menu entry, an
/// input, a button. A row of a browse screen goes through [`row_glyph`], which
/// keeps the titles of songs and records away from here. The one caller that
/// does not hold to that is the empty state, which reads the screen's heading:
/// on Favourites that is the player's word, but on a screen named for a thing
/// — a playlist's page, were one to come back empty — it would be the thing's
/// name, and a playlist called "Save Me" would be drawn with the save glyph.
pub fn glyph_for(title: &str, source: Option<&str>) -> Option<Glyph> {
    reading(title, source, false)
}

/// The glyph for a row of a browse screen, or `None` to draw the player's own
/// picture — or, where it sent none, the note a row shows while its cover is
/// still on its way.
///
/// One answer for the two places that need it: the publisher draws what this
/// returns, and the walk that fetches covers skips every row it returns a
/// glyph for. Worked out in each place separately, the two had already come
/// apart — a picker's chip for a service with no glyph of its own, and the
/// sources on Home's Most Used shelf that name themselves only on their
/// action, were fetched for pictures that were never drawn — and reading a
/// song's title on one side and not the other would have left a row waiting
/// for a cover nothing had asked for.
pub fn row_glyph(section: SectionKind, item: &Item) -> Option<Glyph> {
    let source = item
        .image
        .as_deref()
        .or(item.icon.as_deref())
        .filter(|src| !src.is_empty());

    // What the row is called, for the purpose of choosing an icon for it.
    //
    // Not always the item's own label: Home's Most Used shelf writes Radio
    // Paradise and TuneIn as `<source>` elements with no title at all, and the
    // name comes from the action instead — which is where the caption gets it
    // too. With only the label to go on there was nothing to match and both
    // kept their logos.
    let named = item.label().unwrap_or_else(|| {
        item.action
            .as_ref()
            .and_then(|a| a.title.as_deref())
            .unwrap_or_default()
    });

    // A service picker is a row of names in the app's own chrome, so it gets
    // the app's own icons, and a service's own menu is drawn in glyphs
    // whatever its rows carry, for the reason `menu_glyph` gives. Both are
    // settled before what the row is, so neither turns on the answer.
    if section == SectionKind::SelectorMenu {
        Some(service_glyph(named))
    } else if item.action.as_ref().is_some_and(Action::is_browse_menu) {
        Some(menu_glyph(named))
    } else if item.is_object() {
        object_glyph(source)
    } else {
        glyph_for(named, source)
    }
}

/// The glyph for a row the player presents as a thing — a song, an album, an
/// artist, a station, a playlist — or `None` to use whatever the player sent.
///
/// [`glyph_for`] with the title left out. A thing's title was written by
/// whoever made the record or runs the station, and the vocabulary is the
/// player's names for its own menus and verbs: read against it, "Barometer
/// Song" came out drawn as a track in a list of tracks drawn with the plain
/// note, and a song called "Shuffle" or "Save Me" would have been drawn with
/// the glyph of the button it shares a name with. Which rows are things is the
/// player's to say; see [`Item::is_object`].
///
/// The picture's file name is still read, because it is the player's own word
/// rather than the title's: furniture whose file name says what it is — a
/// playlist drawn with the player's stack of paper — is replaced on a thing as
/// it is anywhere. Any other picture is drawn as it came, and that includes a
/// service's brand mark. Elsewhere a brand mark gives way to the glyph the
/// row's name earns, which is why [`is_content`] does not keep it; a thing's
/// name is not read, so on a thing the mark is what is drawn.
fn object_glyph(source: Option<&str>) -> Option<Glyph> {
    by_path(source.filter(|source| !is_content(source))?)
}

/// The glyph for a row on one of the player's own settings pages, which always
/// gets one.
///
/// The settings vocabulary is read here and nowhere else — see
/// [`settings_vocabulary`] — and a row neither it nor the general reading
/// recognizes falls back to the generic tweak rather than to the player's own
/// picture, which a settings row does not have.
pub fn setting_glyph(label: &str) -> Glyph {
    reading(label, None, true).unwrap_or(Glyph::Tweak)
}

/// The words a settings row is read by.
///
/// Held apart from the rest of the vocabulary and consulted only from
/// [`setting_glyph`], because every word in it is an ordinary English word
/// that turns up in ordinary titles: "Greatest Hits Volume 2" came back drawn
/// with a volume knob, "Audioslave" with a speaker and "Stone Roses" with a
/// tone control. Matching whole words does not save it — *Volume 2* really is
/// the word — but the page the row is on does. On a settings page every row is
/// a control, so the loose matching here is right there and only there.
///
/// `title` arrives lowercased, as the caller already has it.
fn settings_vocabulary(title: &str) -> Option<Glyph> {
    let has = |needle: &str| title.contains(needle);
    let word = |needle: &str| title.split_whitespace().any(|w| w == needle);

    if has("alarm") {
        Some(Glyph::Alarm)
    } else if has("sleep") {
        Some(Glyph::Sleep)
    } else if has("reindex") || has("re-index") {
        Some(Glyph::Rescan)
    } else if has("artwork") {
        Some(Glyph::Artwork)
    } else if has("wifi") || has("wi-fi") || has("wireless") {
        Some(Glyph::Wifi)
    } else if has("network") || has("share") || has("ethernet") {
        Some(Glyph::Network)
    } else if has("server") {
        Some(Glyph::Server)
    } else if has("standby") || has("power") {
        Some(Glyph::Power)
    } else if has("brightness") || has("indicator") || word("dim") {
        Some(Glyph::Brightness)
    } else if has("reset") {
        Some(Glyph::Reset)
    } else if has("tone") || has("treble") || has("bass") || has("crossover") || has("equali") {
        Some(Glyph::Tone)
    } else if has("balance") || has("replay-gain") || has("replay gain") {
        Some(Glyph::Gauge)
    } else if has("volume") || has("subwoofer") || has("output mode") {
        Some(Glyph::Volume)
    } else if has("audio") || has("amplifier") || title == "player" || has("room name") {
        Some(Glyph::Speaker)
    } else {
        None
    }
}

/// The reading both of the above are: `settings` says whether the row is one
/// of the player's own settings rows, which is what decides whether the
/// vocabulary above is in play.
fn reading(title: &str, source: Option<&str>, settings: bool) -> Option<Glyph> {
    // Never override something the player knows better than we do.
    if source.is_some_and(is_content) {
        return None;
    }

    let title = title.to_lowercase();
    let has = |needle: &str| title.contains(needle);
    // Short tokens have to match a whole word. "Search" contains "arc", which
    // is how the search screen came to be labeled with a television.
    let word = |needle: &str| title.split_whitespace().any(|w| w == needle);

    // Inputs first: several of these words also appear in content titles, and
    // an input row is unambiguous because it is what the player calls itself.
    let by_title = if has("bluetooth") {
        Some(Glyph::Bluetooth)
    } else if has("hdmi") || word("arc") || word("tv") || has("television") {
        Some(Glyph::Tv)
    } else if has("optical")
        || has("spdif")
        || has("coax")
        || has("analog")
        || has("analogue")
        || has("aux")
        || has("line in")
        || has("line-in")
    {
        Some(Glyph::Cable)
    } else if word("usb") {
        Some(Glyph::Usb)
    // Context-menu verbs, checked before the nouns inside them: "Add to
    // playlist…" is an add rather than a playlist, and "Play now" is neither.
    // "Queue builder" is the mode where following a row adds it to the queue
    // instead of playing it, so it belongs with the other ways of adding.
    } else if has("add to")
        || has("add next")
        || has("add last")
        || has("add all")
        || has("queue builder")
    {
        Some(Glyph::Enqueue)
    } else if has("play now") || has("play all") {
        Some(Glyph::Play)
    } else if has("shuffle") {
        Some(Glyph::Shuffle)
    } else if has("remove favourite") || has("remove favorite") {
        Some(Glyph::Unfavourite)
    } else if has("technical info") {
        Some(Glyph::Details)
    } else if word("info") {
        Some(Glyph::Info)
    } else if has("customise") || has("customize") || has("manage") || has("setting") {
        Some(Glyph::Settings)
    // The settings pages, before the content vocabulary below: "Music library"
    // is a settings row about the library, not a row of albums, and "Optimize
    // Artwork" is neither an album nor a track. Only on a settings page,
    // though — [`settings_vocabulary`] says why.
    } else if let Some(glyph) = settings.then(|| settings_vocabulary(&title)).flatten() {
        Some(glyph)
    } else if word("edit") {
        Some(Glyph::Edit)
    // "Clear" is the queue's own button; a track's context menu says "Delete
    // from play queue" instead, and without this it kept the player's bitmap
    // bin among a column of Lucide strokes. Removing a favourite is matched
    // further up, so `remove` here cannot steal it.
    } else if word("clear") || word("delete") || word("remove") {
        Some(Glyph::Clear)
    } else if word("save") {
        Some(Glyph::Save)
    } else if has("playlist") {
        Some(Glyph::Playlist)
    } else if has("librar") {
        Some(Glyph::Library)
    } else if has("favourite") || has("favorite") {
        Some(Glyph::Favourite)
    } else if has("preset") {
        Some(Glyph::Preset)
    } else if has("search") {
        Some(Glyph::Search)
    } else if has("recent") || has("history") {
        Some(Glyph::Recent)
    } else if title == "home" {
        Some(Glyph::Home)
    } else if has("news") {
        Some(Glyph::News)
    } else if title == "sources" || has("music source") {
        Some(Glyph::Sources)
    // A radio service's own menu, checked before the content words below,
    // because every row in it also mentions radio or stations: "Local Radio" is
    // somewhere to look rather than a radio, and "Most popular stations" is a
    // chart before it is a station.
    } else if has("for you") || has("recommend") {
        Some(Glyph::ForYou)
    } else if has("sport") {
        Some(Glyph::Sport)
    } else if has("podcast") {
        Some(Glyph::Podcast)
    } else if has("trending") || has("popular") || has("chart") {
        Some(Glyph::Trending)
    } else if has("language") {
        Some(Glyph::Language)
    } else if word("local") || has("near me") || has("nearby") {
        Some(Glyph::Local)
    } else if has("location") || has("region") || has("countr") || word("places") || word("place") {
        Some(Glyph::Place)
    } else if word("music") {
        Some(Glyph::Track)
    } else if has("album") {
        Some(Glyph::Album)
    } else if has("artist") || has("composer") {
        Some(Glyph::Artist)
    } else if has("genre") {
        Some(Glyph::Genre)
    } else if has("folder") {
        Some(Glyph::Folder)
    } else if has("song") || has("track") {
        Some(Glyph::Track)
    } else if has("station") {
        Some(Glyph::Station)
    } else if has("radio paradise") {
        // Both of these come before the general radio test below, which their
        // names would otherwise answer first.
        Some(Glyph::Headphones)
    } else if has("tunein") || has("tune in") {
        Some(Glyph::Broadcast)
    } else if has("radio") {
        Some(Glyph::Radio)
    } else {
        None
    };

    if by_title.is_some() {
        return by_title;
    }

    // Nothing in the title, so fall back to what the player named the file.
    by_path(source?)
}

/// What the player named a picture, read for the few names stable enough to
/// rely on.
fn by_path(source: &str) -> Option<Glyph> {
    let source = source.to_lowercase();
    if source.contains("bluetooth") {
        Some(Glyph::Bluetooth)
    } else if source.contains("ic_tv") {
        Some(Glyph::Tv)
    } else if source.contains("/capture/") {
        Some(Glyph::Cable)
    } else if source.contains("myplaylists") || source.contains("playlist") {
        Some(Glyph::Playlist)
    } else if source.contains("libraryicon") {
        Some(Glyph::Library)
    } else if source.contains("favourite") {
        Some(Glyph::Favourite)
    } else if source.contains("preset") {
        Some(Glyph::Preset)
    } else if source.contains("add") || source.contains("plus") {
        Some(Glyph::Add)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_the_players_furniture() {
        // Every one of these is a real row from a captured browse screen.
        assert_eq!(
            glyph_for("HDMI ARC", Some("/images/capture/ic_tv.png")),
            Some(Glyph::Tv)
        );
        assert_eq!(
            glyph_for("Bluetooth", Some("/images/BluetoothIcon.png")),
            Some(Glyph::Bluetooth)
        );
        assert_eq!(
            glyph_for("Playlists", Some("/images/ci_myplaylists.png")),
            Some(Glyph::Playlist)
        );
        assert_eq!(
            glyph_for("Library", Some("/images/LibraryIcon.png")),
            Some(Glyph::Library)
        );
    }

    #[test]
    fn keeps_anything_the_player_knows_better() {
        // A cover.
        assert_eq!(
            glyph_for(
                "Sticky Fingers",
                Some("/Artwork?service=LocalMusic&album=x")
            ),
            None
        );
        // A station logo from a service's CDN.
        assert_eq!(
            glyph_for("Beyond...", Some("https://img.radioparadise.com/cover.jpg")),
            None
        );
        // A service's own brand mark is replaced, in either of the two places
        // they live. The row already says which service it is.
        assert_eq!(
            glyph_for(
                "Radio Paradise",
                Some("/Sources/images/RadioParadiseIcon.png")
            ),
            Some(Glyph::Headphones)
        );
        // And the general radio glyph is still what everything else with
        // "radio" in its name gets.
        assert_eq!(
            glyph_for("Radio", Some("/Sources/images/AirableIcon.png")),
            Some(Glyph::Radio)
        );
        assert_eq!(
            glyph_for("TuneIn", Some("/images/ui/Source/TuneInSourceIcon.png")),
            Some(Glyph::Broadcast)
        );

        // Still deferred to, and this is the line that matters: a station's
        // own picture and a cover are content, and replacing either would be
        // throwing away the only thing the row has to look at.
        assert_eq!(
            glyph_for("Beyond...", Some("https://img.radioparadise.com/cover.jpg")),
            None
        );
        assert_eq!(
            glyph_for("Gang Shit", Some("/Artwork?service=LocalMusic&fn=x.flac")),
            None
        );
    }

    #[test]
    fn context_menu_verbs_beat_the_nouns_inside_them() {
        // Every one of these is a real row from a captured context menu, and
        // each contains a word that would send it somewhere wrong.
        assert_eq!(
            glyph_for("Add to playlist\u{2026}", None),
            Some(Glyph::Enqueue)
        );
        assert_eq!(glyph_for("Add next", None), Some(Glyph::Enqueue));
        assert_eq!(glyph_for("Play now", None), Some(Glyph::Play));
        assert_eq!(glyph_for("Shuffle", None), Some(Glyph::Shuffle));
        assert_eq!(glyph_for("Info", None), Some(Glyph::Info));
        assert_eq!(glyph_for("Technical info", None), Some(Glyph::Details));
        assert_eq!(glyph_for("Favourite", None), Some(Glyph::Favourite));
        assert_eq!(
            glyph_for("Remove favourite", None),
            Some(Glyph::Unfavourite)
        );
        assert_eq!(glyph_for("Go to album", None), Some(Glyph::Album));
        assert_eq!(glyph_for("Go to artist", None), Some(Glyph::Artist));

        // ...and the plain nouns still land where they should.
        assert_eq!(glyph_for("Playlists", None), Some(Glyph::Playlist));
        assert_eq!(glyph_for("Albums", None), Some(Glyph::Album));
    }

    #[test]
    fn short_words_do_not_match_inside_longer_ones() {
        // "Search" contains "arc"; "Auxiliary" contains "aux" but is one.
        assert_eq!(glyph_for("Search", None), Some(Glyph::Search));
        assert_eq!(glyph_for("HDMI ARC", None), Some(Glyph::Tv));
        assert_eq!(glyph_for("TV", None), Some(Glyph::Tv));
        // A genuine word still matches.
        assert_eq!(glyph_for("Analog 1", None), Some(Glyph::Cable));
    }

    #[test]
    fn a_settings_word_in_an_ordinary_title_is_not_a_settings_row() {
        // Three album titles, and every one of them used to come back drawn
        // as a control off a settings page: a volume knob, a speaker and a
        // tone control.
        assert_eq!(glyph_for("Greatest Hits Volume 2", None), None);
        assert_eq!(glyph_for("Audioslave", None), None);
        assert_eq!(glyph_for("The Stone Roses", None), None);
        // A settings row still reads as what it is, and one nothing
        // recognizes still gets the generic tweak rather than a hole.
        assert_eq!(setting_glyph("Volume limits"), Glyph::Volume);
        assert_eq!(setting_glyph("Audio"), Glyph::Speaker);
        assert_eq!(setting_glyph("Amplifier Standby"), Glyph::Power);
        assert_eq!(setting_glyph("Indicator brightness"), Glyph::Brightness);
        assert_eq!(setting_glyph("Fnordle"), Glyph::Tweak);
        // And the words every row is read by, settings page or not, are
        // still read the same way on one.
        assert_eq!(setting_glyph("Search"), Glyph::Search);
        assert_eq!(setting_glyph("Music library"), Glyph::Library);
    }

    #[test]
    fn falls_back_to_the_path_then_to_nothing() {
        // No word in the title to go on, but the player named the file.
        assert_eq!(
            glyph_for("Input 1", Some("/images/capture/ic_optical.png")),
            Some(Glyph::Cable)
        );
        // Nothing to go on at all: the player's picture stands.
        assert_eq!(glyph_for("Chill Vibes", Some("/images/x9271.png")), None);
        assert_eq!(glyph_for("", None), None);
    }

    #[test]
    fn every_sidebar_screen_gets_one() {
        // A nav list where half the rows have an icon and half do not looks
        // broken, so each screen /ui/Configuration reports must resolve.
        for screen in [
            "Home",
            "Recently Played",
            "News",
            "Favourites",
            "Sources",
            "Search",
            "Presets",
        ] {
            assert!(
                glyph_for(screen, None).is_some(),
                "no glyph for the {screen:?} screen"
            );
        }
    }

    #[test]
    fn a_row_with_no_picture_can_still_have_a_glyph() {
        // The library's own menu entries arrive with no image at all.
        assert_eq!(glyph_for("Albums", None), Some(Glyph::Album));
        assert_eq!(glyph_for("Artists", None), Some(Glyph::Artist));
        assert_eq!(glyph_for("Genres", None), Some(Glyph::Genre));
        assert_eq!(glyph_for("Recently Played", None), Some(Glyph::Recent));
    }

    /// Every row of a screen, by its label, with what [`row_glyph`] draws it
    /// with.
    fn drawn(xml: &str) -> Vec<(String, Option<Glyph>)> {
        let screen = bluos::screen::parse(xml).expect("parses");
        screen
            .sections
            .iter()
            .flat_map(|section| {
                section.items.iter().map(|item| {
                    (
                        item.label().unwrap_or_default().to_owned(),
                        row_glyph(section.kind, item),
                    )
                })
            })
            .collect()
    }

    /// An album's page, made up for these tests in the shape the album page is
    /// drawn from: the cover and what can be done with the whole record in the
    /// header, then a row for each track with its number, its length and its
    /// format, and no picture of its own, the cover being the header's.
    const LOW_PRESSURE: &str = r#"<screen screenTitle="Low Pressure Systems" service="LocalMusic">
  <header image="/Artwork?service=LocalMusic&amp;album=Low+Pressure+Systems" title="Low Pressure Systems" subTitle="Cirrus Fold">
    <button text="Play all"><action type="player-link" URI="/Add?playnow=1&amp;album=1"/></button>
    <button text="Shuffle"><action type="player-link" URI="/Add?playnow=1&amp;shuffle=1&amp;album=1"/></button>
  </header>
  <list>
    <item title="Isobar" track="1" duration="4:12" quality="hd"><action type="player-link" URI="/Add?playnow=1&amp;file=1.flac"/></item>
    <item title="Cold Front" track="2" duration="5:03" quality="hd"><action type="player-link" URI="/Add?playnow=1&amp;file=2.flac"/></item>
    <item title="Dew Point" track="3" duration="3:47" quality="hd"><action type="player-link" URI="/Add?playnow=1&amp;file=3.flac"/></item>
    <item title="Barometer Song" track="4" duration="4:31" quality="hd"><action type="player-link" URI="/Add?playnow=1&amp;file=4.flac"/></item>
    <item title="Altostratus" track="5" duration="6:02" quality="hd"><action type="player-link" URI="/Add?playnow=1&amp;file=5.flac"/></item>
    <item title="Rain Shadow" track="6" duration="4:18" quality="hd"><action type="player-link" URI="/Add?playnow=1&amp;file=6.flac"/></item>
    <item title="Clearing" track="7" duration="5:26" quality="hd"><action type="player-link" URI="/Add?playnow=1&amp;file=7.flac"/></item>
    <item title="High Pressure (Reprise)" track="8" duration="2:54" quality="hd"><action type="player-link" URI="/Add?playnow=1&amp;file=8.flac"/></item>
  </list>
</screen>"#;

    #[test]
    fn every_track_on_an_album_is_drawn_alike() {
        let rows = drawn(LOW_PRESSURE);
        assert_eq!(rows.len(), 8);
        // "Barometer Song" was drawn with the track glyph, for the word in its
        // name, and was the one row in the list that had a glyph at all.
        let first = rows[0].1;
        for (title, glyph) in &rows {
            assert_eq!(*glyph, first, "{title:?} is drawn as its neighbors are");
        }
        assert_eq!(
            first, None,
            "and that is the note a track with no cover of its own is drawn with"
        );

        // The header's buttons are the player's own words, and keep theirs.
        let screen = bluos::screen::parse(LOW_PRESSURE).expect("parses");
        let verbs: Vec<_> = screen
            .header
            .expect("an album's page has a header")
            .buttons
            .iter()
            .map(|button| glyph_for(button.text.as_deref().unwrap_or_default(), None))
            .collect();
        assert_eq!(verbs, [Some(Glyph::Play), Some(Glyph::Shuffle)]);
    }

    /// Titles of songs and records that are also the player's own words. On a
    /// thing, each is drawn as any other thing is; on a menu entry, each is
    /// still read for its word, as it was before.
    #[test]
    fn a_things_title_is_not_read_for_the_players_words() {
        for (title, as_an_entry) in [
            ("Shuffle", Some(Glyph::Shuffle)),
            ("Save Me", Some(Glyph::Save)),
            ("Edit", Some(Glyph::Edit)),
            ("Info", Some(Glyph::Info)),
            ("Home", Some(Glyph::Home)),
            ("News", Some(Glyph::News)),
            ("Clear", Some(Glyph::Clear)),
            ("Radio Song", Some(Glyph::Track)),
            // A word off the settings pages, which only a settings row is read
            // for, so even as an entry this one has none.
            ("Greatest Hits Volume 2", None),
        ] {
            // A track on an album's page, which has no cover of its own.
            let track = format!(
                r#"<screen><list><item title="{title}" track="3" duration="3:30" quality="cd"><action type="player-link" URI="/Add?playnow=1&amp;file=3.flac"/></item></list></screen>"#
            );
            // An album in a list that says what it is, with no cover to hand.
            let album = format!(
                r#"<screen><list><item title="{title}" subTitle="Cirrus Fold" objectType="Album"><action type="browse" URI="/ui/browseContext?service=LocalMusic&amp;type=Album"/></item></list></screen>"#
            );
            // And one on a shelf, known only by its play button and its menu.
            let tile = format!(
                r#"<screen><row title="Recently Added"><item title="{title}" subTitle="Cirrus Fold"><action type="browse" URI="/ui/browseContext?service=LocalMusic&amp;type=Album"/><playAction type="player-link" URI="/Add?playnow=1&amp;album=1"/><contextMenu type="browse" URI="/ui/albumCM" resultType="contextMenu"/></item></row></screen>"#
            );
            for (what, xml) in [("a track", track), ("an album", album), ("a tile", tile)] {
                assert_eq!(
                    drawn(&xml),
                    [(title.to_owned(), None)],
                    "{title:?} as {what}"
                );
            }

            let entry = format!(
                r#"<screen><list><item title="{title}"><action type="browse" URI="/ui/menu"/></item></list></screen>"#
            );
            assert_eq!(
                drawn(&entry),
                [(title.to_owned(), as_an_entry)],
                "{title:?} as a menu entry"
            );
        }
    }

    /// Leaving a thing's title alone leaves its picture to the rules every
    /// picture goes by: a cover is kept, and furniture is replaced where its
    /// file name says what it is.
    #[test]
    fn a_things_picture_is_still_read_by_its_file_name() {
        let rows = drawn(
            r#"<screen><list>
                 <item title="Radio Song Mix" icon="/images/ci_myplaylists.png"><action type="browse" URI="/ui/playlist?id=1"/><contextMenu type="browse" URI="/ui/playlistCM" resultType="contextMenu"/></item>
                 <item title="Shuffle" image="/Artwork?service=LocalMusic&amp;album=Shuffle" objectType="Album"><action type="browse" URI="/ui/album"/></item>
                 <item title="Save Me" image="/images/x9271.png" objectType="Song"/>
               </list></screen>"#,
        );
        assert_eq!(
            rows,
            [
                // Not a track or a radio for its name: the player's stack of
                // paper, and so a playlist.
                ("Radio Song Mix".to_owned(), Some(Glyph::Playlist)),
                // A cover, kept.
                ("Shuffle".to_owned(), None),
                // Nothing in the file name to go on, so the player's picture
                // stands, as it does for any row.
                ("Save Me".to_owned(), None),
            ]
        );
    }

    /// The rows that are not things are drawn through [`row_glyph`] exactly as
    /// they were drawn before it. All but the last three are rows from
    /// captured screens, trimmed.
    #[test]
    fn a_menu_row_is_drawn_as_it_always_was() {
        let rows = drawn(
            r##"<screen>
  <selectorMenu menuTitle="Select Service" replaceScreen="true">
    <item icon="/images/LibraryIcon.png?style=Default" text="Library" selected="true"><action type="browse" URI="/ui/Favourites?service=LocalMusic"/></item>
    <item icon="/Sources/images/TuneInIcon.png?style=Default" text="TuneIn"><action type="browse" URI="/ui/Favourites?service=TuneIn"/></item>
  </selectorMenu>
  <row id="inputs" title="Inputs">
    <input title="HDMI ARC" icon="/images/capture/ic_tv.png"><action type="player-link" URI="/Play?url=Capture%3Ahw%3Aimxspdif&amp;title=HDMI+ARC"/><nowPlayingMatch key="inputId" value="input4"/></input>
  </row>
  <row id="services" title="Music Services"><list>
    <service icon="/images/ui/Source/RadioParadiseSourceIcon.png" title="Radio Paradise"><action type="browse" URI="/ui/browseMenuGroup?service=RadioParadise"/></service>
  </list></row>
  <row id="mostUsed" title="Most Used">
    <source icon="/images/ui/Source/TuneInLogo.png"><action type="browse" URI="/ui/browseMenuGroup?service=TuneIn" title="TuneIn"/></source>
  </row>
  <list>
    <item title="Sports" image="https://cdn.example.com/sports.png"><action type="browse" URI="/ui/BrowseObjects?service=TuneIn&amp;type=BrowseMenu&amp;url=%2FSports"/></item>
    <item title="Albums"><action type="browse" URI="/ui/browseGrouped?service=LocalMusic&amp;type=Album"/></item>
    <item title="Playlists" icon="/images/ci_myplaylists.png"><action type="browse" URI="/ui/playlists"/></item>
  </list>
</screen>"##,
        );
        let expected = [
            ("Library", Some(Glyph::Library)),
            ("TuneIn", Some(Glyph::Broadcast)),
            ("HDMI ARC", Some(Glyph::Tv)),
            ("Radio Paradise", Some(Glyph::Headphones)),
            // Named only on its action, which is what the glyph is chosen by.
            ("", Some(Glyph::Broadcast)),
            // A service's own menu, whose picture is its branding.
            ("Sports", Some(Glyph::Sport)),
            ("Albums", Some(Glyph::Album)),
            ("Playlists", Some(Glyph::Playlist)),
        ]
        .map(|(title, glyph)| (title.to_owned(), glyph));
        assert_eq!(rows, expected);
    }
}
