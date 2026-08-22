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

/// A Lucide glyph to draw instead of the player's own picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    Play,
    Bluetooth,
    Tv,
    Cable,
    Usb,
    Playlist,
    Library,
    Radio,
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
    /// A music service with no glyph of its own — the long tail of them, from
    /// Deezer to whatever the next firmware adds.
    Service,
    /// Only used by the Help menu, which names its own icons.
    Help,
    Rescan,
}

/// Whether `source` is something worth showing as it is.
///
/// Absolute URLs are a service's CDN, `/Artwork` is the player's own art
/// endpoint, and the two service-icon directories hold brand marks.
fn is_content(source: &str) -> bool {
    source.starts_with("http")
        || source.contains("/Artwork")
        || source.contains("/Sources/images/")
        || source.contains("/images/ui/Source/")
}

/// The glyph for a music service, whatever it is called.
///
/// Unlike [`glyph_for`], this never defers to the player's own picture. The
/// sidebar draws the services as a list of names, and the brand marks beside
/// them are PNGs of somebody else's logo; in a column of Lucide strokes they
/// read as a rip in the page. Names it recognises get something apt, and the
/// rest get a note.
pub fn service_glyph(title: &str) -> Glyph {
    glyph_for(title, None).unwrap_or(Glyph::Service)
}

/// The glyph to draw for a row, or `None` to use whatever the player sent.
///
/// The title is matched before the path, because the title is what the reader
/// is looking at: a row called "Albums" should get the album glyph whichever
/// PNG happens to sit beside it.
pub fn glyph_for(title: &str, source: Option<&str>) -> Option<Glyph> {
    // Never override something the player knows better than we do.
    if source.is_some_and(is_content) {
        return None;
    }

    let title = title.to_lowercase();
    let has = |needle: &str| title.contains(needle);
    // Short tokens have to match a whole word. "Search" contains "arc", which
    // is how the search screen came to be labelled with a television.
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
    } else if has("add to") || has("add next") || has("add last") || has("add all") {
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
    // Artwork" is neither an album nor a track.
    } else if has("alarm") {
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
    } else if word("clear") {
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
    } else if has("radio") || has("tunein") || has("tune in") {
        Some(Glyph::Radio)
    } else {
        None
    };

    if by_title.is_some() {
        return by_title;
    }

    // Nothing in the title, so fall back to what the player named the file.
    // Only a few of these are stable enough to rely on.
    let source = source?.to_lowercase();
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
        // A service's own brand mark, in either of the two places they live —
        // "Radio Paradise" would otherwise be caught by the word "radio".
        assert_eq!(
            glyph_for(
                "Radio Paradise",
                Some("/Sources/images/RadioParadiseIcon.png")
            ),
            None
        );
        assert_eq!(
            glyph_for("TuneIn", Some("/images/ui/Source/TuneInSourceIcon.png")),
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
}
