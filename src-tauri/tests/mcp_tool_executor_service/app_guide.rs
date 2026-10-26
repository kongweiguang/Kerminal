//! MCP application navigation contract tests.
//!
//! @author kongweiguang

use super::fixtures::*;

/// Assert the container route stays tied to the focused terminal as the UI entry moves.
#[tokio::test]
async fn mcp_app_guide_places_container_tool_in_right_panel() {
    let (_home, state) = test_state();
    let tools = state.mcp_tool_catalog().list_tools();

    let output = state
        .mcp_tool_executor()
        .execute(
            mcp_context(&state, state.ssh_commands()),
            &tools,
            "kerminal.app_guide",
            json!({}),
        )
        .await
        .expect("app guide");

    assert_eq!(output.status, McpToolExecutionStatus::Succeeded);
    let right_panel = output.data["applicationSurfaces"]
        .as_array()
        .and_then(|surfaces| {
            surfaces
                .iter()
                .find(|surface| surface["surface"] == "rightToolPanel")
        })
        .expect("right tool panel surface");
    let description = right_panel["userSees"]
        .as_str()
        .expect("right panel description");
    assert!(description.contains("Containers"));
    assert!(description.contains("focused terminal host"));
    assert!(description.contains("selecting a host in the left sidebar alone"));
}
