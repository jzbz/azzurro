//! The protocol, exercised end to end against a player that is not there.
//!
//! These are the tests the crate could not have before: every one of them used
//! to need a speaker on the network answering the same way twice. What they
//! cover is the seam between the client and the parsers — that the right path
//! is asked for, with the right parameters, and that the reply is read back
//! into the right shape. The parsers' own edge cases stay in unit tests beside
//! them, where they belong.

use fake_player::{Player, fixtures};

/// Build a client pointed at the fake, with TLS wired up the way the app does.
async fn client_for(player: &Player) -> bluos::client::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    bluos::client::Client::new(player.id()).expect("a client")
}

#[tokio::test]
async fn a_player_that_answers_sync_status_is_a_player() {
    let player = Player::start().await;
    let client = client_for(&player).await;

    let sync = client.sync_status().await.expect("reads");
    assert_eq!(sync.name, "Kitchen");
    assert!(player.asked_for("/SyncStatus"));
}

#[tokio::test]
async fn the_queue_comes_back_as_songs() {
    let player = Player::start().await;
    let client = client_for(&player).await;

    let queue = client.queue().await.expect("reads");
    assert_eq!(queue.length, 1);
    assert_eq!(queue.songs[0].title.as_deref(), Some("A Song"));
}

/// A window onto the queue names its last song, not the one after it. Asked
/// for 5 to 7 a player answers three songs; a client that took `end` as
/// exclusive would skip a song at every seam as the list scrolls.
#[tokio::test]
async fn a_window_onto_the_queue_includes_both_ends() {
    let player = Player::start().await;
    player.hold_queue(10);
    let client = client_for(&player).await;

    let window = client.queue_range(5, 7).await.expect("reads");
    let ids: Vec<u32> = window.songs.iter().map(|s| s.id).collect();
    assert_eq!(ids, [5, 6, 7], "both ends included: {:?}", player.asked());
    assert_eq!(window.length, 10, "length is the whole queue");

    // The last window runs past the end and comes back short.
    let tail = client.queue_range(8, 11).await.expect("reads");
    let ids: Vec<u32> = tail.songs.iter().map(|s| s.id).collect();
    assert_eq!(ids, [8, 9]);

    let whole = client.queue().await.expect("reads");
    assert_eq!(whole.songs.len(), 10, "no window is the whole queue");
}

/// The alarm routes are all one path with different parameters, so what a
/// caller sends is most of what there is to get wrong.
#[tokio::test]
async fn saving_an_alarm_sends_every_field_the_player_wants() {
    let player = Player::start().await;
    let client = client_for(&player).await;

    let alarm = bluos::alarms::Alarm {
        id: 0,
        hour: 6,
        minute: 5,
        days: [false, true, false, true, false, true, false],
        duration: 45,
        volume: 20,
        fade_in: true,
        source: Some("A Station".to_owned()),
        service: Some("TestRadio".to_owned()),
        url: Some("TestRadio:/1".to_owned()),
        ..Default::default()
    };
    client.save_alarm(&alarm).await.expect("saves");

    let sent = player.asked().join(" ");
    assert!(sent.contains("hour=06"), "zero-padded: {sent}");
    assert!(sent.contains("minute=05"));
    assert!(sent.contains("days=0101010"), "Sunday first: {sent}");
    assert!(sent.contains("duration=45"), "an alarm carries a length");
    assert!(!sent.contains("end="), "and not a finishing time");
    assert!(sent.contains("fadein=1"));
    assert!(sent.contains("enable=1"), "a save always arms it");
    assert!(
        sent.contains("tz="),
        "the player has to know whose wall clock"
    );
    assert!(!sent.contains("id="), "a new alarm has no id to send");
}

/// A schedule is the same call with the other half of the pair.
#[tokio::test]
async fn a_schedule_sends_an_end_time_and_no_duration() {
    let player = Player::start().await;
    let client = client_for(&player).await;

    client
        .save_alarm(&bluos::alarms::Alarm {
            id: 3,
            hour: 9,
            minute: 0,
            end: Some("1730".to_owned()),
            duration: 15,
            ..Default::default()
        })
        .await
        .expect("saves");

    let sent = player.asked().join(" ");
    assert!(sent.contains("end=1730"));
    assert!(!sent.contains("duration="), "the two are exclusive: {sent}");
    assert!(sent.contains("id=3"), "an existing alarm names itself");
}

#[tokio::test]
async fn deleting_and_arming_name_the_alarm() {
    let player = Player::start().await;
    // Answer the way a player does, by what was asked: a delete comes back
    // without the alarm, anything else with it.
    player.handle("/Alarms", |request| {
        assert_eq!(request.method, "GET", "every alarm route is a GET");
        Some(match request.param("delete") {
            Some("1") => fixtures::no_alarms().to_owned(),
            _ => fixtures::one_alarm().to_owned(),
        })
    });
    let client = client_for(&player).await;

    let after = client.delete_alarm(4).await.expect("deletes");
    assert!(player.asked_for("delete=1"));
    assert!(player.asked_for("id=4"));
    assert!(
        after.alarms.is_empty(),
        "the reply is the list after the delete"
    );

    let after = client.arm_alarm(4, false).await.expect("disarms");
    assert!(player.asked_for("enable=0"));
    assert_eq!(after.alarms.len(), 1, "arming is not a delete");
}

/// The reply is the whole list, so a caller never re-reads after a write.
#[tokio::test]
async fn a_write_answers_with_the_list() {
    let player = Player::start().await;
    player.serve("/Alarms", fixtures::one_alarm());
    let client = client_for(&player).await;

    let after = client.arm_alarm(1, true).await.expect("arms");
    assert_eq!(after.alarms.len(), 1);
    assert_eq!(after.alarms[0].hour, 7);
    assert!(after.alarms[0].use_backup, "useBackup is written as true");
    assert!(after.alarms[0].fade_in, "fadein is written as 1");
}

/// The one that cost an afternoon: a player-link can answer with a question
/// instead of doing what it was told, and dropping the reply loses both.
#[tokio::test]
async fn a_play_command_can_come_back_as_a_question() {
    let player = Player::start().await;
    player.serve("/ui/prf", fixtures::replace_queue_dialog());
    let client = client_for(&player).await;

    let asked = client
        .follow("/ui/prf?u=%2FAdd%3Fplaynow%3D1")
        .await
        .expect("the request itself succeeds");

    let dialog = asked.expect("the player asked something");
    assert_eq!(dialog.title.as_deref(), Some("Playing replaces Play Queue"));
    assert_eq!(dialog.choices.len(), 2);
    assert!(!dialog.choices[0].is_cancel(), "Replace carries the action");
    assert!(dialog.choices[1].is_cancel(), "Cancel only closes");
}

/// The ordinary case, which must stay ordinary.
#[tokio::test]
async fn a_play_command_that_simply_happens_asks_nothing() {
    let player = Player::start().await;
    player.serve("/Play", "<status/>");
    let client = client_for(&player).await;

    let asked = client.follow("/Play?url=x").await.expect("succeeds");
    assert!(asked.is_none(), "nothing to put in front of anyone");
}

/// Walking the source tree: a folder leads somewhere, a leaf does not.
#[tokio::test]
async fn the_source_tree_is_walked_by_following_rows() {
    let player = Player::start().await;
    let client = client_for(&player).await;

    let top = client
        .stations(bluos::client::Client::station_root())
        .await
        .expect("reads");
    assert_eq!(top.rows.len(), 2);

    let queue = &top.rows[0];
    assert!(queue.playable, "the default is an answer, not a door");
    assert_eq!(queue.into_path(), None);

    let inputs = &top.rows[1];
    assert_eq!(
        inputs.into_path().as_deref(),
        Some("/RadioBrowse?service=Capture&url=presets")
    );
}

/// A player that goes quiet is a failure the caller has to see, not a hang.
#[tokio::test]
async fn a_route_the_player_does_not_have_is_an_error() {
    let player = Player::start().await;
    player.forget("/Alarms");
    let client = client_for(&player).await;

    assert!(client.alarms().await.is_err());
}

/// A settings page id is encoded on the way out, whatever is in it. Ids come
/// out of attributes, so nothing upstream has encoded them already.
#[tokio::test]
async fn a_settings_page_id_is_sent_encoded() {
    let player = Player::with_routes(vec![(
        "/Settings",
        r#"<settings pageId="a b"></settings>"#.to_owned(),
    )])
    .await;
    let client = client_for(&player).await;

    let page = client.settings(Some("a b&c=d")).await.expect("reads");
    assert_eq!(page.page_id.as_deref(), Some("a b"));
    assert!(
        player.asked_for("/Settings?id=a+b%26c%3Dd"),
        "asked {:?}",
        player.asked()
    );

    // And the top of the menu carries no id at all.
    client.settings(None).await.expect("reads");
    assert!(player.asked().iter().any(|seen| seen == "/Settings"));
}

/// Starting a firmware upgrade is the one call in this crate that cannot be
/// undone, so what it refuses matters more than what it does.
#[tokio::test]
async fn an_upgrade_is_refused_unless_the_player_says_it_is_ready() {
    let player = Player::start().await;
    let client = client_for(&player).await;

    // Nothing to install.
    player.serve(
        "/upgrade",
        r#"<upgrade inProgress="false" available="false"/>"#,
    );
    let refused = client.start_upgrade().await;
    assert!(refused.is_err(), "an upgrade nobody has must not start");
    assert!(
        !player.asked_for("upgrade=this"),
        "and the trigger must not have been sent at all"
    );

    // One already running: starting a second is the way to brick a player.
    player.serve(
        "/upgrade",
        r#"<upgrade inProgress="true" available="true"/>"#,
    );
    let refused = client.start_upgrade().await;
    assert!(refused.is_err(), "a second upgrade must not start");
    assert!(!player.asked_for("upgrade=this"));
    // Said as what it is: this crate declining, not a document it misread.
    assert!(
        matches!(&refused, Err(bluos::Error::Refused(why)) if why.contains("already running")),
        "{refused:?}"
    );

    // Ready: available, and nothing running.
    player.serve(
        "/upgrade",
        r#"<upgrade inProgress="false" available="true"/>"#,
    );
    client.start_upgrade().await.expect("starts");
    assert!(
        player.asked_for("upgrade=this"),
        "the scope is this player alone, never all"
    );
    assert!(
        !player.asked_for("upgrade=all"),
        "nothing here may upgrade a whole zone"
    );
    assert!(player.asked_for("upgrade=check"), "and it checked first");
}

/// A filter chosen on a screen is the client's to carry, not the player's to
/// keep.
///
/// Choosing "CD Quality" on Radio Paradise answers 200 with an empty body and
/// an `X-Sovi-Ui-Context` header — base64 JSON naming the filter. Nothing on
/// the player changes. Send the next request without that header and the
/// screen comes back exactly as it was, which is what made the picker look
/// broken: the action ran, the refresh happened, and every request said "no
/// filter" again.
#[tokio::test]
async fn the_players_ui_context_is_carried_back_to_it() {
    let player = Player::start().await;
    let client = client_for(&player).await;

    player.serve("/ui/action", "");
    assert!(client.ui_context().is_none(), "nothing carried yet");

    // The first request goes out without one, because there is none.
    player.serve("/SyncStatus", fixtures::sync_status());
    client.sync_status().await.expect("reads");
    assert!(
        !player.last_head_had("x-sovi-ui-context"),
        "nothing to send, so nothing is sent"
    );

    // Now the player hands one over.
    let context = "eyJ2IjoxLCJmaWx0ZXJzIjp7IlJhZGlvUGFyYWRpc2UtIjoiNCJ9fQo=";
    player.hand_out_context(context);
    client.sync_status().await.expect("reads");
    assert_eq!(
        client.ui_context().as_deref(),
        Some(context),
        "and it is kept"
    );

    // And every request after it carries it back.
    client.sync_status().await.expect("reads");
    assert!(
        player.last_head_had(&format!("x-sovi-ui-context: {context}")),
        "the context must go back or the filter is forgotten: {:?}",
        player.heads().last()
    );
}

/// A reply that says nothing about the context leaves it alone.
///
/// Most requests carry no such header. Treating those as "forget it" would
/// undo a filter on the very next status poll, which is every second.
#[tokio::test]
async fn a_reply_without_a_context_does_not_clear_the_one_held() {
    let player = Player::start().await;
    let client = client_for(&player).await;
    player.serve("/SyncStatus", fixtures::sync_status());

    player.hand_out_context("held");
    client.sync_status().await.expect("reads");
    assert_eq!(client.ui_context().as_deref(), Some("held"));

    // The player stops mentioning it.
    player.state_without_context();
    client.sync_status().await.expect("reads");
    assert_eq!(
        client.ui_context().as_deref(),
        Some("held"),
        "silence is not a retraction"
    );
}

/// Reordering presets sends the whole permutation, as JSON, by POST.
///
/// The shape is not documented anywhere; it was read out of the type errors a
/// Powernode returns for a wrong body. Pinned here because a partial ordering
/// destroys presets, and nothing about the reply says so.
#[tokio::test]
async fn reordering_presets_sends_every_move() {
    let player = Player::start().await;
    let client = client_for(&player).await;
    player.serve("/Presets/edit", "<presets prid=\"5\"></presets>");

    // Dragging the fourth preset to the top.
    let moves = bluos::screen::reordering(&[4, 1, 2, 3]);
    client.reorder_presets(4, &moves).await.expect("reorders");

    assert!(
        player.asked_for("/Presets/edit?prid=4"),
        "the list it is editing is named on the query too: {:?}",
        player.asked()
    );

    let head = player.heads().last().cloned().unwrap_or_default();
    assert!(head.starts_with("POST "), "a GET answers 405: {head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/json"),
        "the player refuses anything else outright"
    );
    let body = player.bodies().last().cloned().unwrap_or_default();
    assert_eq!(
        body,
        r#"{"prid":4,"ordering":[{"from":4,"to":1},{"from":1,"to":2},{"from":2,"to":3},{"from":3,"to":4}]}"#,
        "every slot that moves must be named, or the ones left out are deleted"
    );
}

/// A setting is written as one JSON pair, by POST, to the path it names.
#[tokio::test]
async fn writing_a_setting_posts_its_name_and_value() {
    let player = Player::start().await;
    let client = client_for(&player).await;
    player.serve("/alsa_setting", "<ok/>");

    let setting = bluos::settings::Setting {
        id: "eq-bass".to_owned(),
        name: Some("eq-bass".to_owned()),
        url: Some("/alsa_setting".to_owned()),
        ..Default::default()
    };
    client.write_setting(&setting, "3").await.expect("writes");

    let head = player.heads().last().cloned().unwrap_or_default();
    assert!(head.starts_with("POST /alsa_setting "), "{head}");
    let body = player.bodies().last().cloned().unwrap_or_default();
    assert_eq!(body, r#"{"eq-bass":"3"}"#);
}

/// Nothing to reorder sends nothing at all.
#[tokio::test]
async fn an_order_already_right_asks_the_player_for_nothing() {
    let player = Player::start().await;
    let client = client_for(&player).await;
    player.serve("/Presets/edit", "<presets/>");

    client.reorder_presets(4, &[]).await.expect("does nothing");
    assert!(
        !player.asked_for("/Presets/edit"),
        "an empty permutation is not a request worth making"
    );
}

/// A named stream goes out on the same route a player-link uses.
#[tokio::test]
async fn a_stream_can_be_named_rather_than_browsed_to() {
    let player = Player::start().await;
    let client = client_for(&player).await;

    player.serve("/Play", "<state>stream</state>");
    let asked = client
        .play_url("http://ice1.somafm.com/groovesalad-128-mp3")
        .await
        .expect("plays");
    assert!(asked.is_none(), "an ordinary start asks nothing");

    // Encoded once, by the client, and not twice: a player handed
    // "http%3A%2F%2F..." would look for a host called "http%3A".
    assert!(
        player.asked_for("url=http%3A%2F%2Fice1.somafm.com%2Fgroovesalad-128-mp3"),
        "the stream goes out percent-encoded exactly once: {:?}",
        player.asked()
    );
}

/// Starting a stream can replace the queue, and the player says so instead of
/// doing it. Treating that answer as success loses the question entirely.
#[tokio::test]
async fn a_stream_that_would_replace_the_queue_asks_first() {
    let player = Player::start().await;
    let client = client_for(&player).await;

    player.serve("/Play", fixtures::replace_queue_dialog());
    let asked = client
        .play_url("http://example.com/live.mp3")
        .await
        .expect("answers");
    assert!(asked.is_some(), "the question must survive the call");
}

/// A zone member is addressed through the player that leads it.
///
/// `&slave=<host>&port=<port>` was read out of the official controller rather
/// than observed against a zone — there is one speaker here. What this pins is
/// everything that does not need a zone: that the parameters go out at all,
/// that they carry the member and not the leader, that the check is asked over
/// the same route as the start, and that a refusal still refuses.
#[tokio::test]
async fn a_member_is_upgraded_through_its_leader() {
    let player = Player::start().await;
    let client = client_for(&player).await;
    let member = bluos::DeviceId::new(std::net::Ipv4Addr::new(10, 0, 0, 156), 11000);

    player.serve(
        "/upgrade",
        r#"<upgrade inProgress="false" available="false"/>"#,
    );
    let refused = client.start_upgrade_for(Some(member)).await;
    assert!(refused.is_err(), "a member with nothing to install refuses");
    assert!(
        !player.asked_for("upgrade=this"),
        "and is not sent the trigger"
    );

    player.serve(
        "/upgrade",
        r#"<upgrade inProgress="false" available="true"/>"#,
    );
    client
        .start_upgrade_for(Some(member))
        .await
        .expect("starts");

    assert!(
        player.asked_for("slave=10.0.0.156"),
        "the request names the member"
    );
    assert!(player.asked_for("port=11000"), "and the port it answers on");
    assert!(
        player.asked_for("upgrade=this"),
        "the scope is still this and never all"
    );
    assert!(!player.asked_for("upgrade=all"));

    // The precondition is asked over the same route it will start on: a check
    // about the leader would answer for the wrong player entirely.
    let asked = player.asked();
    let checked = asked
        .iter()
        .find(|seen| seen.contains("upgrade=check"))
        .expect("it checked first");
    assert!(
        checked.contains("slave=10.0.0.156"),
        "the check must be about the member too, not about its leader: {checked}"
    );
}

/// While an upgrade runs the player stops answering with `<SyncStatus>`, which
/// a reader that insists on it would call a broken player.
#[tokio::test]
async fn an_upgrading_player_is_read_as_upgrading_and_not_as_broken() {
    let player = Player::start().await;
    let client = client_for(&player).await;

    player.serve(
        "/SyncStatus",
        r#"<UpgradeStatusStage2 name="Kitchen" model="N330" step="2" total="4"
             percent="55" error="0" abortable="0"/>"#,
    );

    let progress = match client.sync_or_upgrade().await.expect("reads") {
        bluos::client::Sync::Upgrading(progress) => progress,
        bluos::client::Sync::Status(_) => panic!("read an upgrade as an ordinary status"),
    };

    assert_eq!(progress.percent, Some(55));
    assert_eq!(progress.bar(), Some(55));
    assert_eq!(progress.stage(), bluos::upgrade::Stage::Installing);

    // And the ordinary shape still reads as one.
    player.serve("/SyncStatus", fixtures::sync_status());
    assert!(matches!(
        client.sync_or_upgrade().await.expect("reads"),
        bluos::client::Sync::Status(_)
    ));
}
