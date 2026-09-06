use crate::error::Error;
use serde_json::{Map, Value};

pub fn use_json(force_json: bool, stream_is_terminal: bool) -> bool {
    force_json || !stream_is_terminal
}

pub fn render_success(value: &Value, verbose: bool) -> String {
    if value.get("capability_schema_version").is_some() {
        return render_capabilities(value, verbose);
    }
    if value.get("authenticated").is_some() || value.get("checks").is_some() {
        return render_doctor(value, verbose);
    }
    if looks_like_config(value) {
        return render_object("Configuration", value, verbose);
    }
    if let Some(issue) = value.get("issue").filter(|issue| looks_like_issue(issue)) {
        let mut result = render_issue(issue, verbose);
        if let Some(comments) = value.get("comments").and_then(Value::as_array) {
            result.push_str(&format!("\nComments: {}", comments.len()));
        }
        return result;
    }
    if looks_like_issue(value) {
        return render_issue(value, verbose);
    }
    if value.get("sub_issues").is_some()
        && value.get("blocked_by").is_some()
        && value.get("blocking").is_some()
    {
        return render_relationships(value, verbose);
    }
    match value {
        Value::Array(items) if looks_like_sub_issue_tree(items) => {
            render_sub_issue_tree(items, verbose)
        }
        Value::Array(items) => render_array(items, verbose),
        Value::Object(_) => render_object("Result", value, verbose),
        _ => scalar(value),
    }
}

pub fn render_error(error: &Error, verbose: bool) -> String {
    let mut result = format!("Error: {}", error.message);
    if verbose {
        result.push_str(&format!("\nCode: {}", error.code));
        if let Some(status) = error.status {
            result.push_str(&format!("\nHTTP status: {status}"));
        }
        result.push_str(&format!(
            "\nOutcome unknown: {}",
            if error.outcome_unknown { "yes" } else { "no" }
        ));
    }
    result
}

fn looks_like_config(value: &Value) -> bool {
    value.get("github_token_configured").is_some() || value.get("gitlab_token_configured").is_some()
}

fn looks_like_issue(value: &Value) -> bool {
    value.get("number").is_some() && value.get("title").is_some() && resource_url(value).is_some()
}

fn render_capabilities(value: &Value, verbose: bool) -> String {
    let version = value.get("version").map(scalar).unwrap_or_default();
    let schema = value
        .get("capability_schema_version")
        .map(scalar)
        .unwrap_or_default();
    let mut result = format!("issueflow {version}\nCapability schema: {schema}");
    if let Some(platforms) = value.get("platforms").and_then(Value::as_object) {
        result.push_str("\n\nPlatform support\n");
        let rows = platforms
            .iter()
            .map(|(platform, support)| {
                vec![
                    platform.clone(),
                    field(support, "issues"),
                    field(support, "sub_issues"),
                    field(support, "dependencies"),
                    support
                        .get("pull_requests")
                        .or_else(|| support.get("merge_requests"))
                        .map(scalar)
                        .unwrap_or_default(),
                    field(support, "kanban"),
                    field(support, "delivery_recovery"),
                ]
            })
            .collect::<Vec<_>>();
        result.push_str(&table(
            &[
                "PLATFORM",
                "ISSUES",
                "SUB-ISSUES",
                "DEPENDENCIES",
                "PR / MR",
                "KANBAN",
                "DELIVERY",
            ],
            &rows,
        ));
    }
    if let Some(commands) = value.pointer("/cli/subcommands").and_then(Value::as_array) {
        result.push_str("\n\nCommands\n");
        let rows = commands
            .iter()
            .filter_map(|command| command.get("name").and_then(Value::as_str))
            .map(|name| vec![name.to_string()])
            .collect::<Vec<_>>();
        result.push_str(&table(&["COMMAND"], &rows));
    }
    if verbose && let Some(cli) = value.get("cli") {
        result.push_str("\n\nDetails\n");
        render_value(cli, "", &mut result, 0, true);
    }
    result
}

fn render_doctor(value: &Value, verbose: bool) -> String {
    if let Some(checks) = value.get("checks").and_then(Value::as_array) {
        let rows = checks
            .iter()
            .map(|check| {
                vec![
                    field(check, "name"),
                    field(check, "status"),
                    check.get("detail").map(compact_detail).unwrap_or_default(),
                ]
            })
            .collect::<Vec<_>>();
        let mut result = format!(
            "Diagnostics\n{}",
            table(&["CHECK", "STATUS", "DETAIL"], &rows)
        );
        if verbose {
            result.push_str("\n\nDetails\n");
            render_value(value, "", &mut result, 0, true);
        }
        return result;
    }
    render_object("Diagnostics", value, verbose)
}

fn render_relationships(value: &Value, verbose: bool) -> String {
    let mut result = "Relationships".to_string();
    let parent = &value["parent"];
    result.push_str("\nParent: ");
    result.push_str(if parent.is_null() {
        "-"
    } else {
        parent
            .get("html_url")
            .or_else(|| parent.get("webUrl"))
            .or_else(|| parent.get("web_url"))
            .or_else(|| parent.get("url"))
            .and_then(Value::as_str)
            .unwrap_or("present")
    });
    if let Some(summary) = value.get("sub_issues_summary") {
        result.push_str(&format!(
            "\nSub-issues: {} / {} completed",
            field(summary, "completed"),
            field(summary, "total")
        ));
    }
    for (label, key) in [
        ("Sub-issues", "sub_issues"),
        ("Blocked by", "blocked_by"),
        ("Blocking", "blocking"),
    ] {
        result.push_str(&format!("\n\n{label}\n"));
        let items = value[key].as_array().map(Vec::as_slice).unwrap_or(&[]);
        result.push_str(&render_array(items, false));
    }
    if verbose {
        result.push_str("\n\nDetails\n");
        render_value(value, "", &mut result, 0, true);
    }
    result
}

fn looks_like_sub_issue_tree(items: &[Value]) -> bool {
    !items.is_empty()
        && items
            .iter()
            .all(|item| item.get("depth").is_some() && item.get("position").is_some())
}

fn render_sub_issue_tree(items: &[Value], verbose: bool) -> String {
    let mut result = "Sub-issues".to_string();
    for item in items {
        let depth = item["depth"].as_u64().unwrap_or(1).max(1);
        let number = field(item, "number");
        let title = field(item, "title");
        let state = field(item, "state");
        result.push_str(&format!(
            "\n{}- #{} {} [{}]",
            "  ".repeat((depth - 1) as usize),
            number,
            title,
            state
        ));
    }
    if verbose {
        result.push_str("\n\nDetails\n");
        render_value(&Value::Array(items.to_vec()), "", &mut result, 0, true);
    }
    result
}

fn render_issue(value: &Value, verbose: bool) -> String {
    let mut result = format!("#{}  {}", field(value, "number"), field(value, "title"));
    let mut rows = ["state", "platform"]
        .iter()
        .filter_map(|key| value.get(*key).map(|v| vec![label(key), scalar(v)]))
        .collect::<Vec<_>>();
    if let Some(url) = resource_url(value) {
        rows.push(vec!["url".into(), url.into()]);
    }
    if !rows.is_empty() {
        result.push_str("\n\n");
        result.push_str(&table(&["FIELD", "VALUE"], &rows));
    }
    if verbose {
        result.push_str("\n\nDetails\n");
        render_value(value, "", &mut result, 0, true);
    }
    result
}

fn resource_url(value: &Value) -> Option<&str> {
    ["url", "html_url", "web_url", "webUrl"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
}

fn render_array(items: &[Value], verbose: bool) -> String {
    if items.is_empty() {
        return "No results.".into();
    }
    if items.iter().all(Value::is_object) {
        let preferred = ["number", "title", "state", "name", "status", "url"];
        let columns = preferred
            .iter()
            .copied()
            .filter(|key| items.iter().any(|item| scalar_field(item, key).is_some()))
            .collect::<Vec<_>>();
        if !columns.is_empty() {
            let rows = items
                .iter()
                .map(|item| columns.iter().map(|key| field(item, key)).collect())
                .collect::<Vec<Vec<String>>>();
            let headers = columns
                .iter()
                .map(|key| label(key).to_ascii_uppercase())
                .collect::<Vec<_>>();
            let header_refs = headers.iter().map(String::as_str).collect::<Vec<_>>();
            let mut result = table(&header_refs, &rows);
            if verbose {
                result.push_str("\n\nDetails\n");
                render_value(&Value::Array(items.to_vec()), "", &mut result, 0, true);
            }
            return result;
        }
    }
    let mut result = String::new();
    render_value(&Value::Array(items.to_vec()), "", &mut result, 0, verbose);
    result
}

fn render_object(title: &str, value: &Value, verbose: bool) -> String {
    let object = value.as_object().expect("object renderer requires object");
    let rows = object
        .iter()
        .filter(|(_, value)| verbose || is_compact(value))
        .map(|(key, value)| {
            vec![
                label(key),
                if is_sensitive_key(key) {
                    "[redacted]".into()
                } else {
                    summary(value)
                },
            ]
        })
        .collect::<Vec<_>>();
    let mut result = title.to_string();
    if !rows.is_empty() {
        result.push('\n');
        result.push_str(&table(&["FIELD", "VALUE"], &rows));
    }
    if verbose {
        let nested = object.values().any(|value| !is_compact(value));
        if nested {
            result.push_str("\n\nDetails\n");
            render_value(value, "", &mut result, 0, true);
        }
    }
    result
}

fn render_value(value: &Value, key: &str, out: &mut String, depth: usize, verbose: bool) {
    let indent = "  ".repeat(depth);
    match value {
        Value::Object(object) => render_map(object, key, out, depth, verbose),
        Value::Array(items) => {
            if !key.is_empty() {
                push_line(out, &format!("{indent}{}:", label(key)));
            }
            for item in items {
                match item {
                    Value::Object(_) | Value::Array(_) => {
                        render_value(item, "-", out, depth + 1, verbose)
                    }
                    _ => push_line(
                        out,
                        &format!("{}- {}", "  ".repeat(depth + 1), scalar(item)),
                    ),
                }
            }
        }
        _ => push_line(out, &format!("{indent}{}: {}", label(key), scalar(value))),
    }
}

fn render_map(
    object: &Map<String, Value>,
    key: &str,
    out: &mut String,
    depth: usize,
    verbose: bool,
) {
    let indent = "  ".repeat(depth);
    if !key.is_empty() {
        push_line(out, &format!("{indent}{}:", label(key)));
    }
    for (child_key, child) in object {
        if verbose || is_compact(child) {
            if is_sensitive_key(child_key) {
                push_line(
                    out,
                    &format!(
                        "{}{}: [redacted]",
                        "  ".repeat(depth + usize::from(!key.is_empty())),
                        label(child_key)
                    ),
                );
                continue;
            }
            render_value(
                child,
                child_key,
                out,
                depth + usize::from(!key.is_empty()),
                verbose,
            );
        }
    }
}

fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let widths = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            rows.iter()
                .filter_map(|row| row.get(index))
                .map(|value| value.chars().count())
                .chain(std::iter::once(header.chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    let mut output = String::new();
    push_line(&mut output, &table_row(headers.iter().copied(), &widths));
    push_line(
        &mut output,
        &widths
            .iter()
            .map(|width| "-".repeat(*width))
            .collect::<Vec<_>>()
            .join("  "),
    );
    for row in rows {
        push_line(
            &mut output,
            &table_row(row.iter().map(String::as_str), &widths),
        );
    }
    output.trim_end().to_string()
}

fn table_row<'a>(values: impl Iterator<Item = &'a str>, widths: &[usize]) -> String {
    values
        .enumerate()
        .map(|(index, value)| {
            let padding = widths[index].saturating_sub(value.chars().count());
            format!("{value}{}", " ".repeat(padding))
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn scalar_field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key).filter(|value| is_compact(value))
}

fn field(value: &Value, key: &str) -> String {
    if is_sensitive_key(key) {
        return "[redacted]".into();
    }
    scalar_field(value, key).map(scalar).unwrap_or_default()
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    !key.ends_with("_configured")
        && ["token", "authorization", "password", "secret", "cookie"]
            .iter()
            .any(|fragment| key.contains(fragment))
}

fn is_compact(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

fn summary(value: &Value) -> String {
    match value {
        Value::Array(items) => format!("{} item(s)", items.len()),
        Value::Object(items) => format!("{} field(s)", items.len()),
        _ => scalar(value),
    }
}

fn compact_detail(value: &Value) -> String {
    let Some(object) = value.as_object() else {
        return scalar(value);
    };
    let details = object
        .iter()
        .filter(|(key, value)| !is_sensitive_key(key) && is_compact(value))
        .take(3)
        .map(|(key, value)| format!("{}={}", label(key), scalar(value)))
        .collect::<Vec<_>>();
    if details.is_empty() {
        summary(value)
    } else {
        details.join(", ")
    }
}

fn scalar(value: &Value) -> String {
    match value {
        Value::Null => "-".into(),
        Value::Bool(value) => if *value { "yes" } else { "no" }.into(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => value.replace(['\n', '\r'], " "),
        _ => summary(value),
    }
}

fn label(key: &str) -> String {
    key.trim_start_matches('-').replace('_', " ")
}

fn push_line(output: &mut String, line: &str) {
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn capabilities_are_a_scannable_command_table() {
        let output = render_success(
            &json!({"version":"1.0.0","capability_schema_version":1,"cli":{"subcommands":[{"name":"issue"},{"name":"pr"}]}}),
            false,
        );
        assert!(output.contains("issueflow 1.0.0"));
        assert!(output.contains("COMMAND"));
        assert!(output.contains("issue"));
        assert!(!output.contains("subcommands:"));
    }

    #[test]
    fn capabilities_render_platform_support_as_a_matrix() {
        let output = render_success(
            &json!({
                "version":"1.0.0",
                "capability_schema_version":2,
                "platforms":{"github":{"issues":"supported","sub_issues":"supported","dependencies":"supported","pull_requests":"supported","kanban":"Projects","delivery_recovery":"supported"}},
                "cli":{"subcommands":[]}
            }),
            false,
        );
        assert!(output.contains("Platform support"));
        assert!(output.contains("SUB-ISSUES"));
        assert!(output.contains("Projects"));
    }

    #[test]
    fn verbose_capabilities_include_details() {
        let output = render_success(
            &json!({"version":"1.0.0","capability_schema_version":1,"cli":{"name":"issueflow","subcommands":[]}}),
            true,
        );
        assert!(output.contains("Details"));
        assert!(output.contains("name: issueflow"));
    }

    #[test]
    fn empty_arrays_have_an_explicit_message() {
        assert_eq!(render_success(&json!([]), false), "No results.");
    }

    #[test]
    fn issues_have_a_compact_summary() {
        let output = render_success(
            &json!({"number":12,"title":"Improve output","state":"open","platform":"github","url":"https://example.test/issues/12","body":"long"}),
            false,
        );
        assert!(output.starts_with("#12  Improve output"));
        assert!(output.contains("https://example.test/issues/12"));
        assert!(!output.contains("long"));
    }

    #[test]
    fn native_issue_urls_and_comment_wrappers_are_compact() {
        let output = render_success(
            &json!({"issue":{"number":12,"title":"Native","state":"open","html_url":"https://example.test/issues/12","body":"long"},"comments":[]}),
            false,
        );
        assert!(output.starts_with("#12  Native"));
        assert!(output.contains("Comments: 0"));
        assert!(!output.contains("long"));
    }

    #[test]
    fn doctor_check_rows_show_warnings() {
        let output = render_success(
            &json!({"checks":[{"name":"repository","status":"warning","detail":"not configured"}]}),
            false,
        );
        assert!(output.contains("CHECK"));
        assert!(output.contains("warning"));
        assert!(output.contains("not configured"));
    }

    #[test]
    fn doctor_summarizes_structured_check_details() {
        let output = render_success(
            &json!({"checks":[{"name":"kanban","status":"passed","detail":{"ready":true,"project":"https://example.test/project"}}]}),
            false,
        );
        assert!(output.contains("ready=yes"));
        assert!(output.contains("project=https://example.test/project"));
    }

    #[test]
    fn recursive_sub_issues_render_as_a_tree() {
        let output = render_success(
            &json!([
                {"number":2,"title":"child","state":"open","depth":1,"position":1},
                {"number":3,"title":"grandchild","state":"closed","depth":2,"position":1}
            ]),
            false,
        );
        assert!(output.contains("\n- #2 child [open]"));
        assert!(output.contains("\n  - #3 grandchild [closed]"));
    }

    #[test]
    fn verbose_errors_add_safe_metadata() {
        let error = Error::new("configuration", "token is not configured");
        let output = render_error(&error, true);
        assert!(output.contains("Code: configuration"));
        assert!(output.contains("Outcome unknown: no"));
    }

    #[test]
    fn output_mode_is_json_for_pipes_or_an_explicit_flag() {
        assert!(use_json(false, false));
        assert!(use_json(true, true));
        assert!(!use_json(false, true));
    }

    #[test]
    fn verbose_output_redacts_sensitive_fields() {
        let output = render_success(
            &json!({"name":"safe","nested":{"authorization":"Bearer private","access_token":"private","token_configured":true}}),
            true,
        );
        assert!(!output.contains("Bearer private"));
        assert!(!output.contains("access token: private"));
        assert!(output.contains("authorization: [redacted]"));
        assert!(output.contains("token configured: yes"));
    }

    #[test]
    fn representative_lists_render_as_tables_without_color() {
        let output = render_success(
            &json!([{"name":"Backlog","status":"ready","url":"https://example.test/1"}]),
            false,
        );
        assert!(output.contains("NAME"));
        assert!(output.contains("Backlog"));
        assert!(!output.contains("\u{1b}["));
    }
}
