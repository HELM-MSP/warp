use std::time::Duration;

use warp::appearance::Appearance;
use warpui_core::elements::tui::{TuiBufferExt, TuiRect};
use warpui_core::presenter::tui::TuiPresenter;
use warpui_core::App;

use super::{render_response_summary, render_warping_indicator, SPINNER_TIMELINE};

#[test]
fn spinner_follows_the_prototype_choreography() {
    let frame_at = |millis| *SPINNER_TIMELINE.value_at(Duration::from_millis(millis));
    // 180° right at 200ms per 45° step...
    assert_eq!(frame_at(0), "⋮");
    assert_eq!(frame_at(200), "⋰");
    assert_eq!(frame_at(400), "⋯");
    assert_eq!(frame_at(600), "⋱");
    // ...then 180° back left...
    assert_eq!(frame_at(800), "⋮");
    assert_eq!(frame_at(1000), "⋱");
    assert_eq!(frame_at(1200), "⋯");
    assert_eq!(frame_at(1400), "⋰");
    // ...then a rest at vertical before the fast spins...
    assert_eq!(frame_at(1600), "⋮");
    assert_eq!(frame_at(1799), "⋮");
    // ...then fast spins right (540°, three glyph cycles) at 50ms per 45°
    // step.
    assert_eq!(frame_at(1800), "⋰");
    assert_eq!(frame_at(1850), "⋯");
    assert_eq!(frame_at(1900), "⋱");
    assert_eq!(frame_at(1950), "⋮");
    assert_eq!(frame_at(2200), "⋰");
    assert_eq!(frame_at(2300), "⋱");
    // The choreography loops from its full period
    // (8*200 + 200 + 11*50 = 2350ms), the restarting `⋮` completing the
    // final spin.
    assert_eq!(frame_at(2350), "⋮");
    // Each frame holds for its full duration.
    assert_eq!(frame_at(199), "⋮");
}

#[test]
fn renders_the_indicator_row_and_requests_a_repaint() {
    App::test((), |mut app| async move {
        // `TuiUiBuilder` reads theme colors from the `Appearance` singleton.
        app.update(|ctx| {
            ctx.add_singleton_model(|_| Appearance::mock());
        });
        app.read(|app_ctx| {
            let element = render_warping_indicator(Duration::ZERO, app_ctx);
            let mut presenter = TuiPresenter::new();
            let frame = presenter.present_element(element, TuiRect::new(0, 0, 20, 1), app_ctx);

            let lines = frame.buffer.to_lines();
            let line = &lines[0];
            // The spinner glyph could advance a frame on a slow machine, so
            // accept any frame; the label and fresh counter are exact.
            let spinner = line.chars().next().expect("row should not be empty");
            assert!(
                SPINNER_TIMELINE
                    .values()
                    .any(|glyph| *glyph == spinner.to_string().as_str()),
                "unexpected spinner glyph in row: {line:?}"
            );
            assert!(
                line.contains(" Warping (0s)"),
                "unexpected indicator row: {line:?}"
            );

            // The animated row must schedule the next repaint.
            assert!(frame.repaint_at.is_some());
        });
    });
}

#[test]
fn response_summary_shows_duration_and_credits_without_repaints() {
    App::test((), |mut app| async move {
        app.update(|ctx| {
            ctx.add_singleton_model(|_| Appearance::mock());
        });
        app.read(|app_ctx| {
            let element = render_response_summary(Duration::from_secs(5), Some(2.5), app_ctx);
            let mut presenter = TuiPresenter::new();
            let frame = presenter.present_element(element, TuiRect::new(0, 0, 30, 1), app_ctx);

            let lines = frame.buffer.to_lines();
            assert_eq!(lines[0].trim_end(), "∷ 5s • 2.5 credits");
            // The resting row is static: no repaint scheduling.
            assert!(frame.repaint_at.is_none());
        });
    });
}

#[test]
fn response_summary_omits_credits_until_reported() {
    App::test((), |mut app| async move {
        app.update(|ctx| {
            ctx.add_singleton_model(|_| Appearance::mock());
        });
        app.read(|app_ctx| {
            let mut presenter = TuiPresenter::new();
            for block_credits in [None, Some(0.0)] {
                let element =
                    render_response_summary(Duration::from_secs(1), block_credits, app_ctx);
                let frame = presenter.present_element(element, TuiRect::new(0, 0, 30, 1), app_ctx);
                assert_eq!(frame.buffer.to_lines()[0].trim_end(), "∷ 1s");
            }
        });
    });
}
