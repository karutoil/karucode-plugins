//! Browser-free wire-contract tests run in the default suite.

mod support;
use serde_json::json;
use support::PluginProcess;

#[test]
fn initialize_and_every_tool_have_the_expected_contract() {
    let mut plugin = PluginProcess::spawn(None);
    let manifest = plugin.initialize();
    assert_eq!(manifest["name"], "lightpanda");
    assert_eq!(manifest["protocol"], "1");
    assert_eq!(manifest["version"], "0.1.0");
    assert_eq!(manifest["permissions"], json!(["shell", "network"]));
    assert_eq!(manifest["hooks"], json!(["before_prompt"]));

    let result = plugin.request("tools/list", json!({}));
    let tools = result["tools"].as_array().expect("tools array");
    let mut names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "click",
            "evaluate_script",
            "fill",
            "get_console",
            "hover",
            "list_pages",
            "navigate_page",
            "new_page",
            "resize_page",
            "take_screenshot",
            "take_snapshot",
        ]
    );
    for tool in tools {
        assert_eq!(
            tool["parameters"]["type"], "object",
            "{} schema",
            tool["name"]
        );
        assert!(
            tool["description"]
                .as_str()
                .is_some_and(|description| !description.trim().is_empty()),
            "{} description",
            tool["name"]
        );
        assert!(matches!(
            tool["class"].as_str(),
            Some("read_only" | "external")
        ));
    }
    // Deliberately no take_heap_snapshot: this version of Lightpanda does not
    // implement HeapProfiler and a fake/untested compatibility tool is worse.
    let screenshot = tools
        .iter()
        .find(|tool| tool["name"] == "take_screenshot")
        .unwrap();
    assert_eq!(
        screenshot["parameters"]["properties"]["format"]["enum"],
        json!(["png"])
    );
    assert!(screenshot["description"]
        .as_str()
        .unwrap()
        .contains("text-rendered"));
    assert_eq!(screenshot["class"], "external");
}

#[test]
fn argument_errors_are_failed_results_not_plugin_crashes() {
    let mut plugin = PluginProcess::spawn(None);
    plugin.initialize();
    let screenshot = plugin.tool_err("take_screenshot", json!({"format": "jpeg"}));
    assert!(PluginProcess::text(&screenshot).contains("only format=png"));
    let click = plugin.tool_err("click", json!({"uid": "  "}));
    assert!(PluginProcess::text(&click).contains("take_snapshot"));
    let evaluate = plugin.tool_err("evaluate_script", json!({"expression": " "}));
    assert!(PluginProcess::text(&evaluate).contains("expression"));
    let new_page = plugin.tool_err("new_page", json!({"url": ""}));
    assert!(PluginProcess::text(&new_page).contains("url"));

    // Prove these errors didn't terminate or corrupt the protocol loop.
    let again = plugin.request("tools/list", json!({}));
    assert_eq!(again["tools"].as_array().unwrap().len(), 11);
}
