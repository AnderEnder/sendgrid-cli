//! MCP `prompts` — human-invokable, slash-command-style workflow templates. Prompts are
//! **user-controlled** (a person picks them in the client), so they do not fire
//! autonomously; they're a UX convenience that drops the user into the
//! search→describe→invoke discipline. The agent's own always-available guidance lives in
//! `instructions` and the `read_doc` skill, not here.

use rmcp::model::{GetPromptResult, Prompt, PromptArgument, PromptMessage, PromptMessageRole};
use serde_json::{Map, Value};

/// The advertised prompt list (for `prompts/list`).
pub fn list() -> Vec<Prompt> {
    vec![
        Prompt::new(
            "find_operation",
            Some(
                "Find and safely call the SendGrid operation for a goal (search → describe → dry-run invoke).",
            ),
            Some(vec![
                PromptArgument::new("goal")
                    .with_description("What you want to accomplish, e.g. 'create a contact list'.")
                    .with_required(true),
            ]),
        ),
        Prompt::new(
            "safe_invoke",
            Some("Invoke a known operation id safely: describe it, dry-run, then commit."),
            Some(vec![
                PromptArgument::new("id")
                    .with_description("The operation id (or alias) to invoke.")
                    .with_required(true),
            ]),
        ),
    ]
}

/// Render a prompt by name with its arguments (for `prompts/get`). `Err` for an unknown name.
pub fn get(name: &str, args: &Map<String, Value>) -> Result<GetPromptResult, String> {
    let arg = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").trim();
    match name {
        "find_operation" => {
            let goal = arg("goal");
            let text = format!(
                "Goal: {goal}\n\n\
                 Use the SendGrid MCP server to accomplish this:\n\
                 1. search_operations {{ \"query\": \"{goal}\" }} — pick the best-matching op id. If \
                    nothing fits, retry with the modern SendGrid term (campaign\u{2192}single send, \
                    verify\u{2192}validate, suppress\u{2192}suppression).\n\
                 2. describe_operation {{ \"id\": <that id> }} — read required fields, the example, and \
                    constraints.\n\
                 3. invoke_operation {{ \"id\": <that id>, \"dry_run\": true, ... }} — preview first, then \
                    re-invoke without dry_run once the request looks right.\n\n\
                 Read sendgrid://skill/using-the-server (or read_doc) first if you're unsure."
            );
            Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                PromptMessageRole::User,
                text,
            )]))
        }
        "safe_invoke" => {
            let id = arg("id");
            let text = format!(
                "Invoke `{id}` safely:\n\
                 1. describe_operation {{ \"id\": \"{id}\" }} — confirm params and the side-effect class.\n\
                 2. invoke_operation {{ \"id\": \"{id}\", \"dry_run\": true, ... }} — inspect request_preview.\n\
                 3. Re-invoke without dry_run to commit. If you get E_POLICY_DENIED, the server forbids \
                    that side-effect class — do not retry; tell the user."
            );
            Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                PromptMessageRole::User,
                text,
            )]))
        }
        other => Err(format!("unknown prompt: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn list_advertises_both_prompts() {
        let names: Vec<String> = list().into_iter().map(|p| p.name).collect();
        assert!(names.contains(&"find_operation".to_string()));
        assert!(names.contains(&"safe_invoke".to_string()));
    }

    #[test]
    fn get_interpolates_arguments() {
        let mut a = Map::new();
        a.insert("goal".into(), json!("create a contact list"));
        let r = get("find_operation", &a).unwrap();
        let PromptMessage {
            content: rmcp::model::PromptMessageContent::Text { text },
            ..
        } = &r.messages[0]
        else {
            panic!("expected text content");
        };
        assert!(text.contains("create a contact list"));
    }

    #[test]
    fn get_unknown_prompt_errors() {
        assert!(get("nope", &Map::new()).is_err());
    }
}
