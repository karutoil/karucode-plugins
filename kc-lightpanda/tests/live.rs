//! Explicit real-Lightpanda tests; no Chromium or external web service used.
//!
//! `LIGHTPANDA_PATH=/path/to/lightpanda cargo test --test live -- --ignored --test-threads=1`

mod support;

use kc_lightpanda::browser::BrowserManager;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};
use support::{browser_config, data_url, PluginProcess};

const ACTIONS: &str = r#"<!doctype html><title>Action fixture</title>
<button onclick="document.title='clicked';this.dataset.count=String(Number(this.dataset.count||0)+1);console.log('button clicked')">Press me</button>
<label>Name <input aria-label="Name" oninput="this.dataset.input='yes'" onchange="this.dataset.change='yes'"></label>
<textarea aria-label="Comments"></textarea>
<select aria-label="Choice"><option value="a">A</option><option value="b">B</option></select>
<input type="checkbox" aria-label="Agree">
<div role="textbox" contenteditable="true" aria-label="Editable">old</div>
<div role="button" aria-label="Hover target" onmouseover="this.dataset.hovered='yes'">Hover here</div>"#;

fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    predicate()
}

fn http_fixture(html: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Consume the request before closing: unread request bytes can
            // turn close into a TCP reset that discards the buffered response.
            // One response write also exercises the ordinary HTTP fast path.
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0u8; 8192];
            let mut received = 0usize;
            while received < request.len() {
                match stream.read(&mut request[received..]) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        received += n;
                        if request[..received].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let body = html.as_bytes();
            let mut response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            response.extend_from_slice(body);
            let _ = stream.write_all(&response);
        }
    });
    format!("http://{address}/fixture")
}

#[test]
#[ignore = "requires a real Lightpanda binary"]
fn launches_navigates_evaluates_reads_console_and_cleans_up() {
    let mut browser = BrowserManager::new(browser_config());
    assert!(!browser.is_running());
    let page = browser
        .ensure_page(None)
        .expect("start and create initial page");
    assert!(browser.session(page).is_some());
    let version = browser.browser_version().unwrap();
    assert!(version.browser.starts_with("Lightpanda/"), "{version:?}");
    assert!(!version.lightpanda_version.is_empty(), "{version:?}");
    let browser_pid = browser.browser_pid().expect("owned process pid");

    let url = data_url("<title>Navigation fixture</title><h1>Hello Lightpanda</h1>");
    let current = browser
        .navigate(Some(page), &url, 15_000)
        .expect("navigate");
    assert!(current.starts_with("data:text/html,"), "{current}");
    assert_eq!(
        browser.current_title(Some(page)).unwrap(),
        "Navigation fixture"
    );
    let value = browser
        .evaluate(
            Some(page),
            "({marked:document.body.textContent.includes('Hello Lightpanda'), n:6 * 7})",
            false,
        )
        .unwrap();
    assert_eq!(value, json!({"marked": true, "n": 42}));
    let async_value = browser
        .evaluate(
            Some(page),
            "new Promise(resolve => setTimeout(() => resolve({ready:true}), 20))",
            true,
        )
        .unwrap();
    assert_eq!(async_value, json!({"ready": true}));
    let exception = browser
        .evaluate(Some(page), "missing_global_live_test", false)
        .unwrap_err();
    assert!(
        exception.to_string().contains("script threw"),
        "{exception}"
    );
    assert!(browser.ensure_page(Some(99_999)).is_err());

    browser
        .evaluate(
            Some(page),
            "console.log('live log', {answer:42}); console.warn('live warning'); true",
            false,
        )
        .unwrap();
    let messages = browser.take_console(Some(page)).unwrap();
    assert!(
        messages.iter().any(|line| line.contains("live log")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|line| line.contains("live warning")),
        "{messages:?}"
    );
    assert!(
        browser.take_console(Some(page)).unwrap().is_empty(),
        "console did not drain"
    );

    let pages = browser.page_targets().unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].title, "Navigation fixture");
    assert!(browser.screenshot(Some(page), "jpeg", false).is_err());
    let shot = browser.screenshot(Some(page), "png", false).unwrap();
    assert!(shot.len() > 1000, "unexpected tiny PNG: {}", shot.len());
    assert_eq!(&shot[1..4], b"PNG");
    let (width, height) = BrowserManager::screenshot_size(&shot).unwrap();
    assert!(
        width >= 1440 && height >= 900,
        "not desktop-sized: {width}x{height}"
    );

    browser.shutdown();
    assert!(!browser.is_running());
    #[cfg(target_os = "linux")]
    assert!(
        wait_until(|| !support::linux_process_running(browser_pid)),
        "Lightpanda pid {browser_pid} survived shutdown"
    );
}

#[test]
#[ignore = "requires a real Lightpanda binary"]
fn snapshot_uids_drive_click_fill_hover_and_double_click() {
    let mut browser = BrowserManager::new(browser_config());
    let first = browser.create_page(&data_url(ACTIONS), true).unwrap();
    let snapshot = browser.snapshot(Some(first.id), 500).unwrap();
    assert!(snapshot.text.contains("Press me"), "{}", snapshot.text);
    assert!(snapshot.text.contains("uid="), "{}", snapshot.text);
    let find = |role: &str, name: &str| {
        snapshot
            .elements
            .iter()
            .find(|element| element.role == role && element.name == name)
            .unwrap_or_else(|| panic!("missing {role} {name} in {}", snapshot.text))
            .uid
            .clone()
    };
    let button = find("button", "Press me");
    browser.click(Some(first.id), &button, false).unwrap();
    assert_eq!(browser.current_title(Some(first.id)).unwrap(), "clicked");
    assert_eq!(
        browser
            .evaluate(
                Some(first.id),
                "document.querySelector('button').dataset.count",
                false
            )
            .unwrap(),
        json!("1")
    );
    browser.click(Some(first.id), &button, true).unwrap();
    assert_eq!(
        browser
            .evaluate(
                Some(first.id),
                "document.querySelector('button').dataset.count",
                false
            )
            .unwrap(),
        json!("3"),
        "double click should send two click gestures"
    );

    browser
        .fill(Some(first.id), &find("textbox", "Name"), "Ada Lovelace")
        .unwrap();
    browser
        .fill(Some(first.id), &find("textbox", "Comments"), "A note")
        .unwrap();
    browser
        .fill(Some(first.id), &find("combobox", "Choice"), "b")
        .unwrap();
    browser
        .fill(Some(first.id), &find("checkbox", "Agree"), "true")
        .unwrap();
    browser
        .fill(Some(first.id), &find("textbox", "Editable"), "new text")
        .unwrap();
    let filled = browser
        .evaluate(
            Some(first.id),
            "({name:document.querySelector('input[aria-label=Name]').value,comments:document.querySelector('textarea').value,select:document.querySelector('select').value,checked:document.querySelector('input[type=checkbox]').checked,editable:document.querySelector('[contenteditable]').textContent,inputEvent:document.querySelector('input[aria-label=Name]').dataset.input,changeEvent:document.querySelector('input[aria-label=Name]').dataset.change})",
            false,
        )
        .unwrap();
    assert_eq!(
        filled,
        json!({
            "name":"Ada Lovelace", "comments":"A note", "select":"b", "checked":true,
            "editable":"new text", "inputEvent":"yes", "changeEvent":"yes"
        })
    );

    browser
        .hover(Some(first.id), &find("button", "Hover target"))
        .unwrap();
    assert_eq!(
        browser
            .evaluate(
                Some(first.id),
                "document.querySelector('[aria-label=\"Hover target\"]').dataset.hovered",
                false
            )
            .unwrap(),
        json!("yes")
    );
    assert!(browser.click(Some(first.id), "999_999", false).is_err());
    browser
        .navigate(
            Some(first.id),
            &data_url("<title>Fresh document</title><p>new</p>"),
            15_000,
        )
        .unwrap();
    assert!(
        browser.click(Some(first.id), &button, false).is_err(),
        "navigation must invalidate snapshot uids"
    );
}

#[test]
#[ignore = "requires a real Lightpanda binary"]
fn pages_resize_full_page_screenshots_and_storage_are_isolated() {
    let url = http_fixture("<title>HTTP isolation fixture</title><p>storage enabled</p>");
    let mut browser = BrowserManager::new(browser_config());
    let first = browser.create_page(&url, true).unwrap();
    assert_eq!(
        browser
            .evaluate(
                Some(first.id),
                "localStorage.setItem('owner','first');localStorage.getItem('owner')",
                false
            )
            .unwrap(),
        json!("first")
    );
    let mut long_text = std::iter::repeat_n(
        "<p>A full page of textual content should grow beyond the viewport when requested.</p>",
        80,
    )
    .collect::<String>();
    long_text.insert_str(0, "<title>Long fixture</title>");
    // Keep the server response 'static without adding a general HTTP test crate.
    let url2 = http_fixture(Box::leak(long_text.into_boxed_str()));
    let second = browser.create_page(&url2, false).unwrap();
    assert_eq!(first.id + 1, second.id);
    assert_eq!(
        browser.ensure_page(None).unwrap(),
        first.id,
        "foreground=false changed active page"
    );
    assert_eq!(
        browser
            .evaluate(Some(second.id), "localStorage.getItem('owner')", false)
            .unwrap(),
        Value::Null,
        "independent CDP contexts unexpectedly shared localStorage"
    );
    assert_eq!(
        browser
            .evaluate(
                Some(second.id),
                "localStorage.setItem('owner','second');localStorage.getItem('owner')",
                false
            )
            .unwrap(),
        json!("second")
    );
    assert_eq!(
        browser
            .evaluate(Some(first.id), "localStorage.getItem('owner')", false)
            .unwrap(),
        json!("first")
    );
    let pages = browser.page_targets().unwrap();
    assert_eq!(
        pages.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![first.id, second.id]
    );
    assert_eq!(pages[0].title, "HTTP isolation fixture");
    assert_eq!(pages[1].title, "Long fixture");

    let resized = browser
        .resize_page(Some(first.id), 390, 844, Some(1.0), true)
        .unwrap();
    assert_eq!(resized, (390, 844));
    assert_eq!(
        browser
            .evaluate(
                Some(first.id),
                "matchMedia('(max-width:500px)').matches",
                false
            )
            .unwrap(),
        json!(true)
    );
    let shot = browser.screenshot(Some(first.id), "png", false).unwrap();
    assert_eq!(BrowserManager::screenshot_size(&shot), Some((390, 844)));
    let full = browser.screenshot(Some(second.id), "png", true).unwrap();
    let (_, full_height) = BrowserManager::screenshot_size(&full).unwrap();
    assert!(
        full_height > 900,
        "full text screenshot should be taller than 900px: {full_height}"
    );
    browser.clear_viewport(Some(first.id)).unwrap();
    assert_ne!(browser.viewport_size(Some(first.id)).unwrap(), (390, 844));
}

#[test]
#[ignore = "requires a real Lightpanda binary"]
fn plugin_jsonrpc_exercises_every_registered_tool() {
    let browser_path = browser_config().browser_path;
    let screenshot_dir =
        std::env::temp_dir().join(format!("kc-lightpanda-plugin-test-{}", std::process::id()));
    let mut settings = json!({"screenshot_dir": screenshot_dir.to_string_lossy(), "viewport": {"width": 1440, "height": 900}, "max_snapshot_nodes": 200});
    if let Some(path) = browser_path {
        settings["browser_path"] = json!(path);
    }
    let mut plugin = PluginProcess::spawn(Some(settings));
    plugin.initialize();
    let empty = plugin.tool_ok("list_pages", json!({}));
    assert!(PluginProcess::text(&empty).contains("No pages"));

    let page = plugin.tool_ok("new_page", json!({"url":"about:blank"}));
    let page_id = page["meta"]["pageId"].as_i64().unwrap();
    let navigated = plugin.tool_ok(
        "navigate_page",
        json!({"pageId":page_id,"url":data_url(ACTIONS),"timeout_ms":15000}),
    );
    assert!(PluginProcess::text(&navigated).contains("Navigated to data:text/html,"));
    let listed = plugin.tool_ok("list_pages", json!({}));
    assert!(
        PluginProcess::text(&listed).contains(&format!("pageId={page_id}")),
        "{}",
        PluginProcess::text(&listed)
    );

    let snapshot = plugin.tool_ok("take_snapshot", json!({"pageId":page_id,"max_nodes":500}));
    let text = PluginProcess::text(&snapshot).to_string();
    let find_uid = |role_and_name: &str| {
        let line = text
            .lines()
            .find(|line| line.contains(role_and_name))
            .unwrap_or_else(|| panic!("missing {role_and_name}: {text}"));
        let uid = line
            .split("uid=")
            .nth(1)
            .and_then(|s| s.split(']').next())
            .unwrap();
        uid.to_string()
    };
    let button = find_uid(r#"button "Press me""#);
    let input = find_uid(r#"textbox "Name""#);
    plugin.tool_ok("click", json!({"pageId":page_id,"uid":button}));
    plugin.tool_ok(
        "fill",
        json!({"pageId":page_id,"uid":input,"value":"Grace Hopper"}),
    );
    let hover = find_uid(r#"button "Hover target""#);
    plugin.tool_ok("hover", json!({"pageId":page_id,"uid":hover}));
    let evaluated = plugin.tool_ok("evaluate_script", json!({"pageId":page_id,"expression":"({title:document.title,name:document.querySelector('input[aria-label=Name]').value,hovered:document.querySelector('[aria-label=\"Hover target\"]').dataset.hovered})"}));
    assert_eq!(
        evaluated["meta"]["value"],
        json!({"title":"clicked","name":"Grace Hopper","hovered":"yes"})
    );
    let async_eval = plugin.tool_ok("evaluate_script", json!({"pageId":page_id,"expression":"Promise.resolve('async works')","await_promise":true}));
    assert_eq!(PluginProcess::text(&async_eval), "async works");

    plugin.tool_ok(
        "evaluate_script",
        json!({"pageId":page_id,"expression":"console.warn('plugin console works');true"}),
    );
    let console = plugin.tool_ok("get_console", json!({"pageId":page_id}));
    let console_text = PluginProcess::text(&console).to_string();
    assert!(console_text.contains("button clicked"), "{console_text}");
    assert!(
        console_text.contains("plugin console works"),
        "{console_text}"
    );
    assert!(
        PluginProcess::text(&plugin.tool_ok("get_console", json!({"pageId":page_id})))
            .contains("No console")
    );

    let resized = plugin.tool_ok(
        "resize_page",
        json!({"pageId":page_id,"width":420,"height":780,"device_scale_factor":1.0,"mobile":false}),
    );
    assert_eq!(resized["meta"]["actual"], json!({"width":420,"height":780}));
    plugin.tool_ok("resize_page", json!({"pageId":page_id,"clear":true}));
    let screenshot = plugin.tool_ok(
        "take_screenshot",
        json!({"pageId":page_id,"format":"png","full_page":true}),
    );
    let path = screenshot["meta"]["path"].as_str().unwrap();
    let image = std::fs::read(path).unwrap();
    assert!(image.len() > 1000 && image.starts_with(&[0x89, b'P', b'N', b'G']));
    assert_eq!(screenshot["meta"]["renderer"], "text-only");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let bounds = plugin.tool_ok("take_snapshot", json!({"pageId":page_id,"max_nodes":1}));
    assert_eq!(bounds["meta"]["elements"], 1);
    assert!(PluginProcess::text(
        &plugin.tool_err("evaluate_script", json!({"pageId":99999,"expression":"1"}))
    )
    .contains("no page"));
    plugin.kill_now();
    let _ = std::fs::remove_dir_all(&screenshot_dir);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires a real Lightpanda binary and Linux process inspection"]
fn force_killing_plugin_also_stops_its_browser() {
    let mut settings = json!({});
    if let Some(path) = browser_config().browser_path {
        settings["browser_path"] = json!(path);
    }
    let mut plugin = PluginProcess::spawn(Some(settings));
    plugin.initialize();
    plugin.tool_ok("new_page", json!({"url":"about:blank"}));
    let children = support::linux_direct_children(plugin.id());
    assert_eq!(
        children.len(),
        1,
        "expected one directly-owned browser, got {children:?}"
    );
    let browser_pid = children[0];
    assert!(support::linux_process_running(browser_pid));
    plugin.kill_now();
    assert!(
        wait_until(|| !support::linux_process_running(browser_pid)),
        "Lightpanda pid {browser_pid} survived abrupt plugin termination"
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires a real Lightpanda binary and Linux process inspection"]
fn closing_plugin_stdin_gracefully_stops_its_browser() {
    let mut settings = json!({});
    if let Some(path) = browser_config().browser_path {
        settings["browser_path"] = json!(path);
    }
    let mut plugin = PluginProcess::spawn(Some(settings));
    plugin.initialize();
    plugin.tool_ok("new_page", json!({"url":"about:blank"}));
    let children = support::linux_direct_children(plugin.id());
    assert_eq!(
        children.len(),
        1,
        "expected one browser child: {children:?}"
    );
    let browser_pid = children[0];
    assert!(
        plugin.finish_stdin(),
        "plugin did not exit successfully on EOF"
    );
    assert!(
        wait_until(|| !support::linux_process_running(browser_pid)),
        "Lightpanda pid {browser_pid} survived graceful plugin exit"
    );
}
