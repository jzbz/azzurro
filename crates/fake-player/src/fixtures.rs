//! Documents in the shapes a real player serves.
//!
//! Written by hand from documents a Powernode on BluOS 4.16.6 answered with.
//! The shapes are faithful — attribute names, the two spellings of a boolean,
//! the wrapped `/ui/prf?u=` play command — and the contents are invented, so
//! nothing here says anything about anybody's music.

/// What a player answers when nothing much is happening: one speaker, one
/// track queued and stopped, no alarms.
pub fn at_rest() -> Vec<(&'static str, String)> {
    vec![
        ("/SyncStatus", sync_status().to_owned()),
        ("/Status", status_stopped().to_owned()),
        ("/ui/Configuration", configuration().to_owned()),
        ("/ui/Home", home().to_owned()),
        ("/Playlist", queue().to_owned()),
        ("/Alarms", no_alarms().to_owned()),
        ("/RadioBrowse", station_root().to_owned()),
        ("/Settings", settings().to_owned()),
    ]
}

/// What makes an address a player. Answering this is the whole test.
pub fn sync_status() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<SyncStatus etag="a1" id="127.0.0.1:11000" name="Kitchen" model="T100"
            modelName="Test Player" brand="Azzurro" mac="00:11:22:33:44:55"
            icon="/images/players/N125_nt.png" volume="25"
            schemaVersion="35" version="4.16.6"/>"#
}

pub fn status_stopped() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<status etag="1">
  <state>stop</state>
  <service>LocalMusic</service>
  <volume>25</volume>
  <canSeek>0</canSeek>
  <shuffle>0</shuffle>
  <repeat>2</repeat>
</status>"#
}

/// Playing, so that anything gated on "is it making noise" can be exercised.
pub fn status_playing() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<status etag="2">
  <state>play</state>
  <service>LocalMusic</service>
  <title1>A Song</title1>
  <title2>A Band</title2>
  <album>A Record</album>
  <secs>12</secs>
  <totlen>240</totlen>
  <volume>25</volume>
  <canSeek>1</canSeek>
  <quality>cd</quality>
</status>"#
}

/// Which screens the player offers, and the routes for the queue and its
/// context menus.
pub fn configuration() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<configuration>
  <item id="home" uri="/ui/Home"/>
  <item id="favourites" uri="/ui/Favourites"/>
  <item id="search" uri="/ui/Search"/>
  <item id="queue" uri="/ui/playQueue"/>
  <item id="nowPlayingContextMenu" uri="/ui/nowPlayingCM"/>
  <item id="queueItemContextMenu" uri="/ui/queueCM"/>
  <item id="sources" uri="/ui/Sources"/>
</configuration>"#
}

/// A home screen with one shelf of tracks and one of sources, which is enough
/// to exercise a row that plays, a row that opens, and an input.
pub fn home() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<screen screenTitle="Home" id="screen-home">
  <row id="recent" title="Recently Played">
    <item type="thumbnail" text="A Song" subText="A Band" image="/art/1.jpg">
      <action type="player-link" URI="/ui/prf?u=%2FAdd%3Fplaynow%3D1%26file%3D%252Fmusic%252Fa.flac"/>
    </item>
    <item type="thumbnail" text="A Record" subText="A Band" image="/art/2.jpg">
      <action type="browse" URI="/ui/browseContext?service=LocalMusic&amp;type=Album"/>
    </item>
  </row>
  <row id="mostUsed" title="Most Used">
    <source text="Line In" image="/images/in.png">
      <action type="player-link" URI="/Play?url=Capture%3Ahw%3Ain"/>
      <nowPlayingMatch key="inputId" value="input1"/>
    </source>
    <source text="Library" image="/images/lib.png">
      <action type="browse" URI="/ui/browseMenuGroup?service=LocalMusic"/>
    </source>
  </row>
  <customiseScreen text="Customise Home"/>
</screen>"#
}

/// One track, stopped, which is the state most queue tests want.
pub fn queue() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<playlist repeat="0" length="1" id="7" modified="0" shuffle="0">
  <song id="0" service="LocalMusic">
    <art>A Band</art>
    <alb>A Record</alb>
    <title>A Song</title>
    <time>240</time>
    <quality>cd</quality>
  </song>
</playlist>"#
}

pub fn no_alarms() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<alarms supportsEndTime="true"></alarms>"#
}

/// One alarm, carrying every attribute the official controller reads —
/// including the player's two spellings of a boolean.
pub fn one_alarm() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<alarms supportsEndTime="true">
  <alarm id="1" hour="7" minute="30" days="0111110" duration="30" volume="20"
         fadein="1" enable="1" useBackup="true" source="A Station"
         service="TestRadio" url="TestRadio:/1" image="/art/s.jpg"/>
</alarms>"#
}

/// The top of the tree an alarm's source is chosen from.
pub fn station_root() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<radiotime>
  <item text="Current play queue or station" type="audio" key="music"></item>
  <item text="Inputs" type="link" URL="presets" service="Capture" image="/images/in.png"></item>
</radiotime>"#
}

/// The settings root, with the alarms row that leads into the alarms screen.
pub fn settings() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<settings schemaVersion="35">
  <setting id="alarms" displayName="Alarms" class="alarms" count="0" enabled="0"/>
  <setting id="sleep" displayName="Sleep timer" class="sleep" sleep=""/>
</settings>"#
}

/// The reply a player gives instead of acting, when playing would discard a
/// queue that has something in it.
pub fn replace_queue_dialog() -> &'static str {
    r##"<?xml version="1.0" encoding="UTF-8"?>
<dialog title="Playing replaces Play Queue" body="Playing this will clear your existing queue.">
  <button text="Replace" textColor="#FF3B30">
    <action type="player-link" URI="/Add?playnow=1&amp;file=%2Fmusic%2Fa.flac"/>
  </button>
  <button text="Cancel"><action type="nil"/></button>
  <closeAction type="nil"/>
</dialog>"##
}
