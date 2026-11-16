//! Primary viewer rule (roadmap M18): `VideoQoS::apply_primary` and what it changes.
use super::*;
use fast::{Applied, Report, Step};
use primary::GateInfo;

const DELL: i32 = 1;
const QUEST: i32 = 2;

fn input(age_s: u64) -> GateInfo {
    GateInfo {
        control: true,
        input_age: Some(Duration::from_secs(age_s)),
        ..Default::default()
    }
}

/// Two viewers at 30 fps; the Dell (id 1) has control and recent input, the Quest (id 2) has
/// neither; one display, 4000 kbps at the Balanced ratio. Rule on.
fn two_viewers() -> (VideoQoS, HashMap<i32, GateInfo>) {
    let mut qos = stable_qos();
    qos.users.insert(QUEST, UserData::default());
    for _ in 0..12 {
        qos.user_network_delay(QUEST, 10);
    }
    qos.new_display("test".to_owned());
    qos.set_support_changing_quality("test", true);
    qos.store_bitrate(4000);
    qos.advance_ms(4000);
    let infos = HashMap::from([(DELL, input(1)), (QUEST, GateInfo::default())]);
    qos.apply_primary(true, &infos);
    assert!(qos.users[&DELL].policy.primary);
    assert!(!qos.users[&QUEST].policy.primary);
    (qos, infos)
}

#[test]
fn latency_primary_rule_slow_secondary_does_not_lower_fps() {
    let (mut qos, _) = two_viewers();
    qos.user_custom_fps(DELL, 60);
    qos.user_custom_fps(QUEST, 10);
    for _ in 0..12 {
        qos.user_network_delay(DELL, 10);
    }
    let fps = qos.fps();
    assert!(fps > 30, "the primary viewer's cap rules: {fps}");
    // The Quest's delay rises: its own fps drops, the stream's does not.
    for _ in 0..10 {
        qos.user_network_delay(QUEST, 900);
    }
    assert_eq!(qos.fps(), fps);
    assert!(qos.users[&QUEST].delay.fps.unwrap() < fps);
}

#[test]
fn latency_primary_rule_off_keeps_minimum() {
    let (mut qos, infos) = two_viewers();
    qos.apply_primary(false, &infos);
    qos.user_custom_fps(QUEST, 10);
    qos.user_network_delay(QUEST, 10);
    assert_eq!(qos.fps(), 10);
}

#[test]
fn latency_primary_rule_secondary_delay_does_not_cut_ratio() {
    let (mut qos, _) = two_viewers();
    let ratio = qos.ratio;
    for _ in 0..6 {
        qos.user_network_delay(QUEST, 900);
        qos.advance_ms(1000);
        qos.update_display_data("test", 10);
    }
    assert!(qos.ratio >= ratio, "{} < {}", qos.ratio, ratio);
}

#[test]
fn latency_primary_rule_secondary_quality_choice_waits() {
    let (mut qos, _) = two_viewers();
    // No primary viewer chose a quality yet: the Quest's choice is the stream's.
    qos.user_image_quality(QUEST, ImageQuality::Low.value());
    assert_eq!(qos.latest_quality(), Quality::Low);
    qos.user_image_quality(DELL, ImageQuality::Balanced.value());
    let ratio = qos.ratio;
    qos.user_image_quality(QUEST, ImageQuality::Low.value());
    assert_eq!(qos.ratio, ratio);
    assert_eq!(qos.latest_quality(), Quality::Balanced);
    qos.user_image_quality(DELL, ImageQuality::Best.value());
    assert_eq!(qos.latest_quality(), Quality::Best);
}

#[test]
fn latency_primary_rule_secondary_fast_cut_refused() {
    let (mut qos, _) = two_viewers();
    let r = Report {
        epoch: qos.fast_epoch,
        set_kbps: Some(1000.0),
        step: Some(Step::Cut { react_ms: 100.0 }),
    };
    assert_eq!(qos.on_fast(QUEST, r), Applied::Refused("secondary"));
}

#[test]
fn latency_primary_rule_lagging_after_two_seconds_behind() {
    let (mut qos, mut infos) = two_viewers();
    let behind = GateInfo {
        peak: primary::LAG_QUEUE_FRAMES,
        ..Default::default()
    };
    infos.insert(QUEST, behind);
    qos.apply_primary(true, &infos);
    assert!(!qos.users[&QUEST].policy.lagging);
    qos.apply_primary(true, &infos);
    assert!(qos.users[&QUEST].policy.lagging);
    infos.insert(QUEST, GateInfo::default());
    for _ in 0..primary::LAG_LEAVE_SECS - 1 {
        qos.apply_primary(true, &infos);
    }
    assert!(qos.users[&QUEST].policy.lagging);
    qos.apply_primary(true, &infos);
    assert!(!qos.users[&QUEST].policy.lagging);
}

#[test]
fn latency_primary_rule_primary_never_lagging_and_all_primary_without_input() {
    let (mut qos, mut infos) = two_viewers();
    let behind = GateInfo {
        control: true,
        input_age: Some(Duration::from_secs(1)),
        peak: 10,
        ..Default::default()
    };
    infos.insert(DELL, behind);
    for _ in 0..3 {
        qos.apply_primary(true, &infos);
    }
    assert!(!qos.users[&DELL].policy.lagging);
    // Input older than 30 s: nobody is primary by input, so everybody is.
    infos.insert(DELL, input(31));
    infos.insert(
        QUEST,
        GateInfo {
            peak: 10,
            ..Default::default()
        },
    );
    for _ in 0..3 {
        qos.apply_primary(true, &infos);
    }
    assert!(qos
        .users
        .values()
        .all(|u| u.policy.primary && !u.policy.lagging));
}

#[test]
fn latency_primary_rule_floor_for_lowered_custom_fps() {
    let (mut qos, infos) = two_viewers();
    // Nobody primary by input: both count; the Quest logged in at 30 and halves to 15.
    let all = HashMap::from([(DELL, input(60)), (QUEST, infos[&QUEST])]);
    qos.apply_primary(true, &all);
    qos.user_login_custom_fps(QUEST, 30);
    qos.user_custom_fps(QUEST, 30);
    qos.user_custom_fps(QUEST, 15);
    qos.user_network_delay(QUEST, 10);
    assert_eq!(qos.users[&QUEST].fps_cap(), 24);
    assert!(qos.fps() >= 24 || qos.fps() == INIT_FPS);
    // Rule off: the request is taken as it is.
    qos.apply_primary(false, &all);
    assert_eq!(qos.users[&QUEST].fps_cap(), 15);
}

#[test]
fn latency_primary_rule_new_viewer_not_primary_while_one_is_by_input() {
    let (mut qos, _) = two_viewers();
    qos.on_connection_open(3);
    assert!(!qos.users[&3].policy.primary);
    // Its start-up guard does not hold the stream at INIT_FPS.
    qos.user_custom_fps(DELL, 60);
    for _ in 0..12 {
        qos.user_network_delay(DELL, 10);
    }
    assert!(qos.fps() > INIT_FPS);
}

#[test]
fn latency_primary_rule_floor_without_login_custom_fps() {
    // A stock viewer at Balanced quality sends no custom_fps at login; its first custom_fps is
    // its fps controller's first reduction, which must not become the reference.
    let (mut qos, _) = two_viewers();
    qos.on_connection_open(3);
    qos.apply_primary(true, &HashMap::new());
    qos.user_custom_fps(3, 15);
    assert_eq!(qos.users[&3].fps_cap(), 24);
    qos.user_custom_fps(3, 10);
    assert_eq!(qos.users[&3].fps_cap(), 24);
    // A login custom_fps below the floor is its own floor.
    qos.user_login_custom_fps(3, 20);
    assert_eq!(qos.users[&3].fps_cap(), 20);
    qos.apply_primary(false, &HashMap::new());
    assert_eq!(qos.users[&3].fps_cap(), 10);
}
