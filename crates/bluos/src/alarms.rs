//! The alarms and schedules a player will wake up for.
//!
//! `GET /Alarms` answers with the lot, and the same route with different
//! parameters is also how they are made, changed, switched off and deleted —
//! see [`crate::client::Client::alarms`] and its neighbours. Every reply is
//! this document, so a caller never has to re-read after a write.
//!
//! ```xml
//! <alarms supportsEndTime="true">
//!   <alarm id="1" hour="7" minute="30" days="0111110" duration="30"
//!          volume="25" fadein="1" enable="1" useBackup="true"
//!          source="Radio Paradise" service="RadioParadise"
//!          url="RadioParadise:/0:0/Main+Mix" image="http://…/cover.jpg"/>
//! </alarms>
//! ```
//!
//! An empty `<alarms supportsEndTime="true"></alarms>` is what a player with
//! none set answers, and is not an error.
//!
//! The attribute names and their spellings come from the official controller's
//! own parser, read out of the shipped JavaScript, because there is no
//! document for any of this. Note that the player is not consistent about how
//! it writes a boolean — `fadein` and `enable` are `1`, while `useBackup`,
//! `shuffle` and `canShuffle` are `true` — so every one of them goes through
//! [`crate::xml::flag`], which takes either.

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::error::{Error, Result};
use crate::xml::{attributes, flag, local_name};

/// How many days a week has, which is how long `days` is.
pub const DAYS: usize = 7;

/// One alarm, or one schedule.
///
/// The difference is [`Self::end`]: an alarm plays for [`Self::duration`]
/// minutes, a schedule plays until a wall-clock time. A player says whether it
/// can do the second at all with [`Alarms::supports_end_time`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Alarm {
    /// The player's own handle for it. Needed to change or delete one; a new
    /// alarm is saved without one and the player allocates it.
    pub id: u32,
    /// When it starts, on a 24-hour clock.
    pub hour: u8,
    pub minute: u8,
    /// When a schedule stops, as the player writes it: `"HHmm"`, no separator.
    /// `None` on an alarm, which stops after `duration` instead.
    pub end: Option<String>,
    /// Which days it repeats on, Sunday first. All false means it happens once.
    pub days: [bool; DAYS],
    /// Minutes to play, on an alarm. The official controller offers 15, 30,
    /// 45, 60, 90 and 120, and defaults to 15.
    pub duration: u32,
    /// 0 to 100. The controller defaults a new one to 25, on the reasoning
    /// that being woken at whatever the speaker was last left at is unkind.
    pub volume: u32,
    /// Whether it comes up gradually rather than starting at `volume`.
    pub fade_in: bool,
    pub shuffle: bool,
    /// Whether this source can be shuffled at all — a stream cannot.
    pub can_shuffle: bool,
    /// Whether it is armed. An alarm can be switched off without losing it.
    pub enabled: bool,
    /// Fall back to the player's built-in tone if the source cannot be
    /// reached. The controller sets this on alarms and not on schedules: an
    /// alarm that fails silently has failed at its one job.
    pub use_backup: bool,
    /// What it plays, as the player describes it: a display name, the service
    /// it belongs to, the URL to play, and a picture.
    pub source: Option<String>,
    pub service: Option<String>,
    pub url: Option<String>,
    pub image: Option<String>,
}

impl Alarm {
    /// Whether this runs to a finishing time rather than for a length.
    pub fn is_schedule(&self) -> bool {
        self.end.is_some()
    }

    /// The days as the wire writes them: seven characters of `1` and `0`.
    pub fn days_field(&self) -> String {
        self.days
            .iter()
            .map(|on| if *on { '1' } else { '0' })
            .collect()
    }

    /// Whether it repeats at all, as opposed to going off once.
    pub fn repeats(&self) -> bool {
        self.days.iter().any(|on| *on)
    }
}

/// Everything `GET /Alarms` says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Alarms {
    /// Whether this player can do schedules — an alarm with a finishing time —
    /// as opposed to alarms alone. Announced on the root element.
    pub supports_end_time: bool,
    pub alarms: Vec<Alarm>,
}

/// One numeric attribute, or zero.
///
/// Absent and unreadable are the same answer here: the player writes these on
/// every alarm it has, so anything else is a firmware that has moved on, and a
/// zero is a visible wrong value rather than a refused document.
fn number<T: std::str::FromStr + Default>(
    a: &mut std::collections::BTreeMap<String, String>,
    key: &str,
) -> T {
    a.remove(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or_default()
}

/// Read the document.
pub fn parse(xml: &str) -> Result<Alarms> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut out = Alarms::default();
    let mut seen_root = false;

    loop {
        let e = match reader.read_event() {
            Err(e) => return Err(Error::Screen(format!("alarms: {e}"))),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => e,
            Ok(_) => continue,
        };

        let qname = e.name();
        let name = local_name(qname.as_ref());
        let mut a = attributes(&e);

        match name {
            "alarms" => {
                out.supports_end_time = flag(a.remove("supportsEndTime"));
                seen_root = true;
            }
            // Refused rather than skipped: a reply that is not this document
            // is the player disagreeing with us about the route, and reading
            // it as "no alarms" would quietly show an empty list.
            _ if !seen_root => {
                return Err(Error::Screen(format!("alarms: root is <{name}>")));
            }
            "alarm" => {
                let mut days = [false; DAYS];
                if let Some(written) = a.remove("days") {
                    for (slot, c) in days.iter_mut().zip(written.chars()) {
                        *slot = c == '1';
                    }
                }
                // Zero is not a neutral default here: it is the sentinel for
                // an alarm that does not exist on the player yet, which is
                // what makes the editor offer "Create" and what makes
                // `save_alarm` omit the id. An alarm whose id is missing or
                // unreadable would arrive wearing that meaning, and saving it
                // would create a duplicate instead of replacing it — so it is
                // dropped rather than shown as something it is not.
                let Some(id) = a
                    .remove("id")
                    .and_then(|id| id.parse::<u32>().ok())
                    .filter(|id| *id != 0)
                else {
                    continue;
                };
                out.alarms.push(Alarm {
                    id,
                    hour: number(&mut a, "hour"),
                    minute: number(&mut a, "minute"),
                    // Kept as written. It is a wall-clock time in the player's
                    // own format and goes back out the same way.
                    end: a.remove("end").filter(|end| !end.is_empty()),
                    days,
                    duration: number(&mut a, "duration"),
                    volume: number(&mut a, "volume"),
                    fade_in: flag(a.remove("fadein")),
                    shuffle: flag(a.remove("shuffle")),
                    can_shuffle: flag(a.remove("canShuffle")),
                    enabled: flag(a.remove("enable")),
                    use_backup: flag(a.remove("useBackup")),
                    source: a.remove("source"),
                    service: a.remove("service"),
                    url: a.remove("url"),
                    image: a.remove("image"),
                });
            }
            _ => {}
        }
    }

    // A body with no `<alarms>` in it at all — an empty reply, or a redirect
    // page. Returning the default would draw an empty list and call it the
    // truth, which is the same silence this crate has been bitten by before.
    if !seen_root {
        return Err(Error::Screen("alarms: no <alarms> in the reply".to_owned()));
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a Powernode with nothing set answers, captured verbatim.
    #[test]
    fn a_player_with_no_alarms_is_not_an_error() {
        let out = parse(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<alarms supportsEndTime=\"true\"></alarms>",
        )
        .expect("parses");
        assert!(out.alarms.is_empty());
        assert!(out.supports_end_time, "the player says it can do schedules");
    }

    /// Zero is how `save_alarm` is told to create rather than replace, so an
    /// alarm that arrives wearing it by accident would be offered to the user
    /// as "Create" and would duplicate itself on save.
    #[test]
    fn an_alarm_whose_id_is_unusable_is_dropped_rather_than_shown_as_new() {
        let out = parse(
            r#"<alarms supportsEndTime="true">
                 <alarm hour="7" minute="0" enable="1"/>
                 <alarm id="" hour="8" minute="0" enable="1"/>
                 <alarm id="not-a-number" hour="9" minute="0" enable="1"/>
                 <alarm id="0" hour="10" minute="0" enable="1"/>
                 <alarm id="4" hour="11" minute="0" enable="1"/>
               </alarms>"#,
        )
        .expect("parses");

        assert_eq!(
            out.alarms.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![4],
            "only the alarm with a usable id survives"
        );
        assert_eq!(out.alarms[0].hour, 11, "and it is the right one");
    }

    /// An alarm carrying every attribute the official controller reads.
    #[test]
    fn one_alarm_with_everything_on_it() {
        let out = parse(
            r#"<alarms supportsEndTime="true">
                 <alarm id="3" hour="7" minute="5" days="0111110" duration="30"
                        volume="25" fadein="1" enable="1" useBackup="true"
                        shuffle="true" canShuffle="true"
                        source="Radio Paradise" service="RadioParadise"
                        url="RadioParadise:/0:0/Main" image="http://x/c.jpg"/>
               </alarms>"#,
        )
        .expect("parses");

        assert_eq!(out.alarms.len(), 1);
        let alarm = &out.alarms[0];
        assert_eq!(alarm.id, 3);
        assert_eq!((alarm.hour, alarm.minute), (7, 5));
        // Sunday first, so this is the weekdays.
        assert_eq!(
            alarm.days,
            [false, true, true, true, true, true, false],
            "days are Sunday-first"
        );
        assert_eq!(alarm.days_field(), "0111110", "and go back out as written");
        assert!(alarm.repeats());
        assert_eq!(alarm.duration, 30);
        assert_eq!(alarm.volume, 25);
        // `1` here and `true` there, both meaning yes.
        assert!(alarm.fade_in, "fadein is written as 1");
        assert!(alarm.enabled, "enable is written as 1");
        assert!(alarm.use_backup, "useBackup is written as true");
        assert!(alarm.shuffle && alarm.can_shuffle);
        assert_eq!(alarm.source.as_deref(), Some("Radio Paradise"));
        assert_eq!(alarm.service.as_deref(), Some("RadioParadise"));
        assert!(!alarm.is_schedule(), "no end time makes it an alarm");
    }

    /// A schedule is the same element with a finishing time on it.
    #[test]
    fn an_end_time_makes_it_a_schedule() {
        let out = parse(
            r#"<alarms supportsEndTime="true">
                 <alarm id="1" hour="9" minute="0" end="1730" days="0000000"
                        volume="10" enable="1"/>
               </alarms>"#,
        )
        .expect("parses");
        let alarm = &out.alarms[0];
        assert!(alarm.is_schedule());
        assert_eq!(alarm.end.as_deref(), Some("1730"));
        assert!(!alarm.repeats(), "no day set means it happens once");
        assert!(!alarm.fade_in, "absent is off");
        assert!(!alarm.use_backup);
    }

    /// A `days` string the player never writes should not panic or bleed.
    #[test]
    fn a_days_field_of_the_wrong_length_is_survivable() {
        let short = parse(r#"<alarms><alarm id="1" days="11"/></alarms>"#).expect("parses");
        assert_eq!(
            short.alarms[0].days,
            [true, true, false, false, false, false, false]
        );

        let long =
            parse(r#"<alarms><alarm id="1" days="111111111111"/></alarms>"#).expect("parses");
        assert_eq!(long.alarms[0].days, [true; DAYS], "the extra is dropped");

        let missing = parse(r#"<alarms><alarm id="1"/></alarms>"#).expect("parses");
        assert_eq!(missing.alarms[0].days, [false; DAYS]);
    }

    /// Something that is not this document at all.
    #[test]
    fn a_reply_from_somewhere_else_is_refused() {
        assert!(parse("<status><state>play</state></status>").is_err());
        assert!(parse("").is_err(), "and so is nothing");
    }
}
