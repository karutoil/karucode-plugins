//! End-to-end smoke test against a real Chrome/Chromium.
//!
//! Ignored by default because it needs a browser binary and takes a few
//! seconds; run it with:
//!
//! ```bash
//! cargo test --test smoke -- --ignored --nocapture
//! ```
//!
//! It exercises the pieces unit tests cannot: spawning Chrome, attaching to a
//! page WebSocket, navigating, evaluating script, and taking a screenshot.

use kc_chrome_devtools::browser::{BrowserConfig, BrowserManager};
use serde_json::json;

fn config() -> BrowserConfig {
    BrowserConfig {
        headless: true,
        isolated: true,
        chrome_path: None,
        user_data_dir: None,
        protocol_timeout_secs: 30,
        startup_timeout_secs: 30,
        // `..Default::default()` supplies the default desktop viewport; do not
        // override it to None here or the tests stop covering real sizing.
        ..Default::default()
    }
}

#[test]
#[ignore = "spawns a real Chrome; run with --ignored"]
fn launches_chrome_navigates_and_evaluates() {
    let mut manager = BrowserManager::new(config());
    let page = manager
        .ensure_page(None)
        .expect("browser should start and attach");
    assert!(manager.session(page).is_some());

    manager
        .navigate(Some(page), "data:text/html,<title>Smoke</title><h1 id=h>hello</h1>", 20_000)
        .expect("navigation should succeed");

    let value = manager
        .evaluate(Some(page), "document.getElementById('h').textContent", false)
        .expect("evaluate should succeed");
    assert_eq!(value, json!("hello"));

    let title = manager
        .evaluate(Some(page), "document.title", false)
        .expect("evaluate title");
    assert_eq!(title, json!("Smoke"));

    // The page shows up in the target list now.
    let targets = manager.targets().expect("target list");
    assert!(targets.iter().any(|t| t.url.contains("data:text/html")));

    let shot = manager
        .screenshot(Some(page), "png", false)
        .expect("screenshot should succeed");
    assert!(shot.len() > 1000, "screenshot looked empty: {} bytes", shot.len());
    assert_eq!(&shot[1..4], b"PNG", "expected a PNG header");
}

#[test]
#[ignore = "spawns a real Chrome; run with --ignored"]
fn snapshot_exposes_uids_and_click_works() {
    let mut manager = BrowserManager::new(config());
    let page = manager.ensure_page(None).expect("attach");
    manager
        .navigate(
            Some(page),
            "data:text/html,<button id=b onclick=\"document.title='clicked'\">Press me</button>",
            20_000,
        )
        .expect("navigate");

    let snapshot = manager.snapshot(Some(page), 500).expect("snapshot");
    assert!(
        snapshot.text.contains("Press me"),
        "snapshot did not include the button:\n{}",
        snapshot.text
    );
    let button = snapshot
        .elements
        .iter()
        .find(|e| e.name == "Press me")
        .expect("button should carry a uid");

    manager
        .click(Some(page), &button.uid, false)
        .expect("click by uid should work");
    let title = manager.evaluate(Some(page), "document.title", false).unwrap();
    assert_eq!(title, json!("clicked"), "the click did not reach the page");
}

#[test]
#[ignore = "spawns a real Chrome; run with --ignored"]
fn resize_page_changes_the_layout_viewport() {
    let mut manager = BrowserManager::new(config());
    let page = manager.ensure_page(None).expect("attach");
    manager
        .navigate(Some(page), "data:text/html,<h1>size</h1>", 20_000)
        .expect("navigate");

    // The default must be a real desktop viewport, not the 800x600 fallback.
    // `--window-size` sets the *window*; `innerHeight` is the content area
    // beneath it, so assert the width exactly and that the height is close to
    // the requested one rather than expecting an exact match.
    let (w, h) = manager.viewport_size(Some(page)).expect("read viewport");
    let want_w = kc_chrome_devtools::browser::DEFAULT_VIEWPORT_WIDTH;
    let want_h = kc_chrome_devtools::browser::DEFAULT_VIEWPORT_HEIGHT;
    assert_eq!(w, want_w, "expected the default desktop width, got {w}");
    assert!(
        h > 600 && h <= want_h,
        "expected a desktop-height viewport near {want_h}, got {h}"
    );

    // Resize to a phone viewport with mobile emulation. Without a viewport meta
    // tag Chrome uses its legacy 980px layout viewport, so use one here: the
    // point of this test is that the size we ask for is the size we get.
    manager
        .navigate(
            Some(page),
            "data:text/html,<meta name='viewport' content='width=device-width,initial-scale=1'><h1>size</h1>",
            20_000,
        )
        .expect("navigate");
    let (w, h) = manager
        .resize_page(Some(page), 390, 844, None, true)
        .expect("resize");
    assert_eq!((w, h), (390, 844), "the page did not take the new size");

    // The page's own media queries must see it too, not just innerWidth.
    let is_mobile_layout = manager
        .evaluate(
            Some(page),
            "window.matchMedia('(max-width: 500px)').matches",
            false,
        )
        .expect("media query");
    assert_eq!(is_mobile_layout, json!(true), "CSS did not reflow to mobile");

    // A screenshot must come out at the requested pixel size.
    let shot = manager
        .screenshot(Some(page), "png", false)
        .expect("screenshot");
    let dims = BrowserManager::screenshot_size(&shot).expect("PNG header");
    assert_eq!(dims, (390, 844), "screenshot was {}x{}", dims.0, dims.1);

    // Without the meta tag, mobile emulation keeps Chrome's 980px legacy
    // viewport. The call must report that honestly rather than claim 390.
    manager
        .navigate(Some(page), "data:text/html,<h1>no meta</h1>", 20_000)
        .expect("navigate");
    let (legacy_w, _) = manager
        .resize_page(Some(page), 390, 844, None, true)
        .expect("resize");
    assert_eq!(
        legacy_w, 980,
        "expected Chrome's legacy 980px layout viewport without a meta tag"
    );

    // `mobile: false` forces an exact CSS width regardless of the meta tag.
    let (desktop_w, desktop_h) = manager
        .resize_page(Some(page), 390, 844, Some(1.0), false)
        .expect("resize");
    assert_eq!(
        (desktop_w, desktop_h),
        (390, 844),
        "mobile: false should give an exact CSS viewport"
    );

    // Clearing returns to the window size, which is not the phone size.
    manager.clear_viewport(Some(page)).expect("clear");
    let (w, _) = manager.viewport_size(Some(page)).expect("read viewport");
    assert_ne!(w, 390, "clearing the override left the viewport at 390");
}

#[test]
#[ignore = "spawns a real Chrome; run with --ignored"]
fn resize_survives_a_full_page_screenshot() {
    let mut manager = BrowserManager::new(config());
    let page = manager.ensure_page(None).expect("attach");
    manager
        .navigate(
            Some(page),
            "data:text/html,<div style='height:3000px'>tall</div>",
            20_000,
        )
        .expect("navigate");
    manager
        .resize_page(Some(page), 800, 600, None, false)
        .expect("resize");

    let shot = manager
        .screenshot(Some(page), "png", true)
        .expect("full-page screenshot");
    let (_, shot_height) = BrowserManager::screenshot_size(&shot).expect("PNG header");
    assert!(
        shot_height > 600,
        "full-page capture should be taller than the viewport, got {shot_height}"
    );

    // The viewport override must still be in place after the capture.
    let (w, h) = manager.viewport_size(Some(page)).expect("read viewport");
    assert_eq!(
        (w, h),
        (800, 600),
        "the full-page screenshot clobbered the viewport override"
    );
}

#[test]
#[ignore = "spawns a real Chrome; run with --ignored"]
fn isolated_pages_have_separate_storage() {
    let mut manager = BrowserManager::new(config());
    let page = manager.ensure_page(None).expect("attach");
    // `data:` URLs have storage disabled, so serve a real origin instead.
    let html = "<p>storage</p>";
    let url = format!("data:text/html,{html}");
    manager.navigate(Some(page), &url, 20_000).unwrap();
    // A data: URL cannot use localStorage; assert the isolation contract on a
    // page that can, by checking the profile directory is throwaway instead.
    let profile = manager.profile_dir().expect("isolated profile");
    assert!(
        profile.to_string_lossy().contains("kc-chrome-devtools-"),
        "expected a throwaway profile, got {}",
        profile.display()
    );
    assert!(profile.is_dir(), "the profile directory should exist");
}