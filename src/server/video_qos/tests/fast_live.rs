//! Fast mode (roadmap M16): `VideoQoS::on_fast` and the TestDelay loop beside it.
use super::*;
use fast::{Applied, Report, Step};

/// One viewer (id 1) at 30 fps, one display, 4000 kbps at the Balanced ratio (0.67).
fn fast_session() -> VideoQoS {
    let mut qos = stable_qos();
    qos.new_display("test".to_owned());
    qos.set_support_changing_quality("test", true);
    qos.store_bitrate(4000);
    qos.first_reply_adjusts_ratio = true;
    qos.advance_ms(4000);
    qos
}

fn report(qos: &VideoQoS, set_kbps: f64, step: Option<Step>) -> Report {
    Report {
        epoch: qos.fast_epoch,
        set_kbps: Some(set_kbps),
        step,
    }
}

fn cut() -> Option<Step> {
    Some(Step::Cut { react_ms: 120.0 })
}

fn inc(factor: f64) -> Option<Step> {
    Some(Step::Increase { factor })
}

fn kbps(qos: &VideoQoS) -> f64 {
    (qos.ratio * qos.kbps_per_ratio().unwrap()) as f64
}

#[test]
fn latency_fast_live_cut_at_once_increase_bounded_by_lowest_target() {
    let mut qos = fast_session();
    qos.users.insert(2, UserData::default());
    let r = report(&qos, 4000.0, None);
    assert_eq!(qos.on_fast(1, r), Applied::Kept);
    assert_eq!(qos.on_fast(2, r), Applied::Kept);
    assert_eq!(qos.ratio(), BR_BALANCED);
    // Viewer 1's cut applies at once.
    let applied = qos.on_fast(1, report(&qos, 2000.0, cut()));
    assert!(matches!(applied, Applied::Ratio { .. }), "{applied:?}");
    assert!((kbps(&qos) - 2000.0).abs() < 1.0, "{}", kbps(&qos));
    assert!(qos.take_fast_cut().is_some_and(|(id, _, _)| id == 1));
    // Viewer 2's increase cannot pass viewer 1's target.
    qos.advance_ms(200);
    assert_eq!(
        qos.on_fast(2, report(&qos, 4160.0, inc(1.04))),
        Applied::Refused("min")
    );
    // Viewer 1's own increase: not within 100 ms of the last change.
    qos.advance_ms(50);
    let _ = qos.on_fast(1, report(&qos, 2080.0, None));
    assert!(matches!(
        qos.on_fast(1, report(&qos, 2080.0, inc(1.04))),
        Applied::Ratio { .. }
    ));
    qos.advance_ms(50);
    assert_eq!(
        qos.on_fast(1, report(&qos, 2163.0, inc(1.04))),
        Applied::Refused("spacing")
    );
    qos.advance_ms(100);
    assert!(matches!(
        qos.on_fast(1, report(&qos, 2163.0, inc(1.04))),
        Applied::Ratio { .. }
    ));
    assert!((kbps(&qos) - 2163.0).abs() < 2.0, "{}", kbps(&qos));
    // A cut above the stream changes nothing.
    assert_eq!(
        qos.on_fast(2, report(&qos, 3000.0, cut())),
        Applied::Refused("lower")
    );
}

#[test]
fn latency_fast_live_testdelay_loop_is_a_backstop() {
    let mut qos = fast_session();
    // Without a fast connection a dynamic screen raises the ratio every 3 s.
    qos.ratio = 0.5;
    qos.update_display_data("test", 30);
    assert!(qos.ratio() > 0.5);
    // With one, the TestDelay loop makes no increase.
    let mut qos = fast_session();
    qos.ratio = 0.5;
    let _ = qos.on_fast(1, report(&qos, 4000.0, None));
    qos.update_display_data("test", 30);
    assert_eq!(qos.ratio(), 0.5);
    // A fast cut; confirmed TestDelay congestion within 2 s of it does not cut again.
    let applied = qos.on_fast(1, report(&qos, 2000.0, cut()));
    assert!(matches!(applied, Applied::Ratio { .. }), "{applied:?}");
    let fast_ratio = qos.ratio();
    qos.advance_ms(100);
    for _ in 0..3 {
        qos.user_network_delay(1, 400);
    }
    assert_eq!(qos.ratio(), fast_ratio);
    // The fps waits for the bitrate floor.
    assert_eq!(qos.fps(), FPS);
    // After 2 s the backstop cuts.
    qos.advance_ms(2100);
    qos.user_network_delay(1, 400);
    assert!(qos.ratio() < fast_ratio, "{} {}", qos.ratio(), fast_ratio);
    assert_eq!(qos.fps(), FPS);
    // A second of excess lowers the fps at once.
    qos.user_network_delay(1, 1500);
    assert!(qos.fps() < FPS);
}

#[test]
fn latency_fast_live_no_increase_while_a_viewer_shows_delay() {
    let mut qos = fast_session();
    qos.users.insert(2, UserData::default());
    let _ = qos.on_fast(1, report(&qos, 4000.0, None));
    let _ = qos.on_fast(1, report(&qos, 3000.0, cut()));
    qos.advance_ms(500);
    // Viewer 2 (a stock viewer, TestDelay only) replies with 400 ms of excess.
    qos.user_network_delay(2, 10);
    qos.user_network_delay(2, 400);
    qos.user_network_delay(2, 400);
    assert_eq!(
        qos.on_fast(1, report(&qos, 3120.0, inc(1.04))),
        Applied::Refused("delay")
    );
}

#[test]
fn latency_fast_live_quality_choice_voids_old_reports() {
    let mut qos = fast_session();
    let _ = qos.on_fast(1, report(&qos, 4000.0, None));
    let old = report(&qos, 2000.0, cut());
    qos.user_image_quality(1, ImageQuality::Best.value());
    assert_eq!(qos.ratio(), BR_BEST);
    assert!(!qos.fast_live());
    assert_eq!(qos.on_fast(1, old), Applied::Refused("epoch"));
    assert_eq!(qos.ratio(), BR_BEST);
    // The estimator's next report, with the new epoch, registers again.
    let ceil = kbps(&qos);
    assert_eq!(qos.on_fast(1, report(&qos, ceil, None)), Applied::Kept);
    assert!(qos.fast_live());
}

#[test]
fn latency_fast_live_refused_without_vbr_or_with_several_displays() {
    let mut qos = fast_session();
    qos.new_display("second".to_owned());
    qos.set_support_changing_quality("second", true);
    assert_eq!(
        qos.on_fast(1, report(&qos, 2000.0, cut())),
        Applied::Refused("displays")
    );
    assert!(!qos.fast_live());
    assert_eq!(qos.ratio(), BR_BALANCED);
    let mut qos = fast_session();
    qos.abr_config = false;
    assert_eq!(
        qos.on_fast(1, report(&qos, 2000.0, cut())),
        Applied::Refused("vbr")
    );
    // Shadow again (no target): the connection leaves the fast set.
    let mut qos = fast_session();
    let _ = qos.on_fast(1, report(&qos, 4000.0, None));
    assert!(qos.fast_live());
    let none = Report {
        epoch: qos.fast_epoch,
        set_kbps: None,
        step: None,
    };
    assert_eq!(qos.on_fast(1, none), Applied::Kept);
    assert!(!qos.fast_live());
}

#[test]
fn latency_fast_live_context_converts_at_the_stored_ratio() {
    let mut qos = fast_session();
    let ctx = qos.fast_context(1, true);
    assert_eq!(ctx.configured_kbps, 4000);
    assert_eq!(ctx.stream_kbps, 4000);
    assert!(ctx.live);
    assert_eq!(ctx.displays, 1);
    // A cut the encoder has not taken yet: the stream already stands for it.
    let _ = qos.on_fast(1, report(&qos, 4000.0, None));
    let _ = qos.on_fast(1, report(&qos, 2000.0, cut()));
    let ctx = qos.fast_context(1, true);
    assert_eq!(ctx.configured_kbps, 4000);
    assert_eq!(ctx.stream_kbps, 2000);
    qos.user_auto_adjust_fps(1, 24);
    assert_eq!(qos.fast_context(1, true).auto_fps, 24);
}

/// hwcodec's H.264 bitrate for `base` x `ratio` (`HwRamEncoder::calc_bitrate`, desktop): x2 up
/// to 2000 kbps, then a factor decaying toward 1.
fn hw_h264(base: f32, ratio: f32) -> u32 {
    let b = base * ratio;
    let factor = if b > 2000.0 {
        1.0 + 1.0 / (1.0 + (b - 2000.0) * 0.001)
    } else {
        2.0
    };
    (b * factor) as u32
}

/// One viewer, 4K NVENC H.264 at Balanced: the encoder's bitrate is not linear in the ratio.
fn hw_session() -> VideoQoS {
    let mut qos = fast_session();
    let base = 5000.0;
    qos.set_bitrate_curve(Some(Box::new(move |r| hw_h264(base, r))));
    let ratio = qos.ratio();
    qos.store_bitrate_at(hw_h264(base, ratio), ratio);
    qos
}

// The encoder takes the ratio at its next frame (check_qos).
fn encoder_takes(qos: &mut VideoQoS) {
    let ratio = qos.ratio();
    let kbps = (qos.bitrate_curve.as_ref().unwrap())(ratio);
    qos.store_bitrate_at(kbps, ratio);
}

#[test]
fn latency_fast_live_hw_curve_converts_cuts_exactly() {
    let mut qos = hw_session();
    let start = qos.bitrate() as f64;
    assert_eq!(qos.fast_context(1, true).stream_kbps as f64, start);
    // A SEVERE halving and then 10 % overuse cuts land where the estimator asked, although
    // the slope at the set point is not the curve's below it.
    let mut expect = start;
    for (i, share) in [0.5, 0.9, 0.9, 0.9].into_iter().enumerate() {
        qos.advance_ms(300);
        expect *= share;
        let applied = qos.on_fast(1, report(&qos, expect, cut()));
        assert!(matches!(applied, Applied::Ratio { .. }), "{i}: {applied:?}");
        encoder_takes(&mut qos);
        let got = qos.bitrate() as f64;
        assert!((got - expect).abs() <= 0.01 * expect + 2.0, "{i}: {got} vs {expect}");
        let ctx = qos.fast_context(1, true);
        assert!((ctx.stream_kbps as f64 - got).abs() <= 2.0, "{i}: {ctx:?}");
        // The estimator's own ratio is the stream's: its increase is not refused "min".
        assert!((qos.users[&1].fast_ratio.unwrap() - qos.ratio).abs() < 1e-3);
    }
    qos.advance_ms(300);
    let up = qos.bitrate() as f64 * 1.04;
    let applied = qos.on_fast(1, report(&qos, up, None));
    assert_eq!(applied, Applied::Kept);
    assert!(matches!(
        qos.on_fast(1, report(&qos, up, inc(1.04))),
        Applied::Ratio { .. }
    ));
}

#[test]
fn latency_fast_live_hw_curve_untrusted_falls_back_to_linear() {
    let mut qos = fast_session();
    // A curve that does not reproduce the stored bitrate (a clamped or refused set): linear.
    qos.set_bitrate_curve(Some(Box::new(|r| hw_h264(5000.0, r))));
    assert!(qos.trusted_curve().is_none());
    let r = qos.ratio_for_kbps(2000.0).unwrap();
    assert!((r - 2000.0 / qos.kbps_per_ratio().unwrap()).abs() < 1e-6);
}
