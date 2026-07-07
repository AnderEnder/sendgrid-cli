//! Contract for the hoisted `sendgrid_core::describe` module — the op-description
//! logic shared by the MCP `describe_operation` tool and the CLI `--explain` flag.

use sendgrid_core::Registry;
use sendgrid_core::describe::describe;

#[test]
fn describe_returns_example_and_response_menu() {
    let reg = Registry::global();

    // A body-bearing op: the synthesized (constraint-repaired) request-body example
    // must be present so the CLI/MCP can show a runnable body.
    let create = reg.by_id("sg_templates_CreateTemplateVersion").unwrap();
    let d = describe(create);
    assert!(!d.params.is_empty(), "params should be listed");
    assert!(!d.invoke_hint.is_empty(), "invoke_hint should be set");
    assert!(
        d.example.is_object(),
        "a body op must synthesize an example object, got {}",
        d.example
    );

    // A GET op: no request body (example is null) but a response field-menu is
    // present so an agent can chain calls / learn the query root.
    let get = reg.by_id("sg_templates_GetTemplateVersion").unwrap();
    let g = describe(get);
    assert!(g.example.is_null(), "a GET has no request-body example");
    assert!(
        !g.response_fields.is_null(),
        "a GET with a response schema must expose a response field-menu"
    );
}
