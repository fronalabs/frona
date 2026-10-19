use std::fmt::Write;

use clap::{Arg, Command};
use frona_api_types::mcp::BridgeToolInfo;

use crate::BridgeBackend;

const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_HELP_COLUMN_WIDTH: usize = 80;

/// The same stdout, stderr and exit status are used by the executable and tool.
#[derive(Default)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

impl CommandOutput {
    fn stdout(&mut self, text: std::fmt::Arguments<'_>) {
        append_output(&mut self.stdout, &mut self.stdout_truncated, text);
    }

    fn stderr(&mut self, text: std::fmt::Arguments<'_>) {
        append_output(&mut self.stderr, &mut self.stderr_truncated, text);
    }
}

fn append_output(output: &mut String, truncated: &mut bool, text: std::fmt::Arguments<'_>) {
    writeln!(OutputWriter { output, truncated }, "{text}")
        .expect("bounded output writes cannot fail");
}

struct OutputWriter<'a> {
    output: &'a mut String,
    truncated: &'a mut bool,
}

impl std::fmt::Write for OutputWriter<'_> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        const TRUNCATED: &str = "\n[Output truncated at 64 KiB.]\n";
        if *self.truncated {
            return Ok(());
        }
        let remaining = MAX_OUTPUT_BYTES - TRUNCATED.len() - self.output.len();
        if text.len() <= remaining {
            self.output.push_str(text);
        } else {
            let mut end = remaining;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.output.push_str(&text[..end]);
            self.output.push_str(TRUNCATED);
            *self.truncated = true;
        }
        Ok(())
    }
}

pub async fn run(client: impl BridgeBackend, args: Vec<String>) -> CommandOutput {
    let mut output = CommandOutput::default();
    output.success = match run_inner(client, args, &mut output).await {
        Ok(success) => success,
        Err(error) => {
            output.stderr(format_args!("error: {error}"));
            false
        }
    };
    output
}

async fn run_inner(
    client: impl BridgeBackend,
    args: Vec<String>,
    output: &mut CommandOutput,
) -> Result<bool, crate::client::Error> {
    let positionals: Vec<&str> = args
        .iter()
        .skip(1)
        .take_while(|a| !a.starts_with('-'))
        .map(|s| s.as_str())
        .collect();

    if positionals
        .iter()
        .skip(usize::from(positionals.first() == Some(&"help")))
        .take(2)
        .any(|name| matches!(*name, "." | ".."))
    {
        return Err(crate::Error::Invalid("Invalid bridge path segment"));
    }

    let has_help = args
        .iter()
        .any(|a| a == "--help" || a == "-h" || a == "help");

    match (positionals.first().copied(), positionals.get(1).copied()) {
        (None, _) | (Some("help"), None) => {
            print_top_level_help(output);
            Ok(true)
        }
        (Some("list"), _) => {
            let servers = bounded_response(client.list_servers().await?)?;
            if servers.is_empty() {
                output.stdout(format_args!("No MCP servers available."));
            } else {
                for s in &servers {
                    if output.stdout_truncated {
                        break;
                    }
                    let desc = s.description.as_deref().unwrap_or("");
                    output.stdout(format_args!(
                        "{:<20} {:<30} ({} tools)  {}",
                        s.handle, s.display_name, s.tool_count, desc
                    ));
                }
            }
            Ok(true)
        }
        (Some("help"), Some(server)) => {
            if let Some(tool) = positionals.get(2) {
                print_tool_help_by_name(&client, server, tool, output).await
            } else {
                print_server_help(&client, server, output).await?;
                Ok(true)
            }
        }
        (Some(server), Some("help")) => {
            print_server_help(&client, server, output).await?;
            Ok(true)
        }
        (Some(server), None) if has_help => {
            print_server_help(&client, server, output).await?;
            Ok(true)
        }
        (Some(server), Some(tool)) if has_help => {
            print_tool_help_by_name(&client, server, tool, output).await
        }
        (Some(server), Some(tool)) => call_tool(&client, server, tool, &args, output).await,
        (Some(server), None) => {
            output.stderr(format_args!(
                "Missing tool name. Run: mcpctl {server} --help"
            ));
            Ok(false)
        }
    }
}

fn print_top_level_help(output: &mut CommandOutput) {
    output.stdout(format_args!(
        "\
mcpctl — CLI bridge for MCP server tools

USAGE:
    mcpctl <SERVER> <TOOL> [OPTIONS]
    mcpctl list

DISCOVERY:
    mcpctl list                          List available servers
    mcpctl <server> --help               List tools on a server
    mcpctl <server> <tool> --help        Show tool parameters

EXAMPLES:
    mcpctl github list_repos --owner octocat
    mcpctl slack send_message --channel \"#general\" --text \"Hello\"

ENVIRONMENT:
    FRONA_TOKEN_FILE    Path to JWT auth token file (required)
    FRONA_API_URL       Base URL of the frona server (required)"
    ));
}

async fn print_server_help(
    client: &impl BridgeBackend,
    server: &str,
    output: &mut CommandOutput,
) -> Result<(), crate::client::Error> {
    let detail = bounded_response(client.server_tools(server).await?)?;
    let desc = detail.description.as_deref().unwrap_or("");

    output.stdout(format_args!("{} — {}", detail.handle, desc));
    output.stdout(format_args!(""));
    output.stdout(format_args!("TOOLS:"));

    let max_name = detail
        .tools
        .iter()
        .map(|t| t.name.len())
        .max()
        .unwrap_or(10)
        .min(MAX_HELP_COLUMN_WIDTH);
    for t in &detail.tools {
        if output.stdout_truncated {
            break;
        }
        output.stdout(format_args!(
            "    {:<width$}  {}",
            t.name,
            t.description,
            width = max_name
        ));
    }

    output.stdout(format_args!(""));
    output.stdout(format_args!("USAGE:"));
    output.stdout(format_args!(
        "    mcpctl {} <TOOL> [OPTIONS]",
        detail.handle
    ));
    output.stdout(format_args!("    mcpctl {} <TOOL> --help", detail.handle));
    Ok(())
}

async fn print_tool_help_by_name(
    client: &impl BridgeBackend,
    server: &str,
    tool: &str,
    output: &mut CommandOutput,
) -> Result<bool, crate::client::Error> {
    let detail = bounded_response(client.server_tools(server).await?)?;
    if let Some(t) = detail.tools.iter().find(|t| t.name == tool) {
        print_tool_help(server, t, output);
        Ok(true)
    } else {
        output.stderr(format_args!("Unknown tool '{tool}' on server '{server}'."));
        Ok(false)
    }
}

fn print_tool_help(server: &str, tool: &BridgeToolInfo, output: &mut CommandOutput) {
    output.stdout(format_args!(
        "{server} {} — {}",
        tool.name, tool.description
    ));
    output.stdout(format_args!(""));

    let props = tool
        .input_schema
        .get("properties")
        .and_then(|v| v.as_object());
    let required: Vec<&str> = tool
        .input_schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    if let Some(props) = props {
        output.stdout(format_args!("OPTIONS:"));
        let max_name = props
            .keys()
            .map(|k| k.len())
            .max()
            .unwrap_or(10)
            .min(MAX_HELP_COLUMN_WIDTH);
        for (name, schema) in props {
            if output.stdout_truncated {
                break;
            }
            let type_str = schema_type_label(schema);
            let desc = schema
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let req_marker = if required.contains(&name.as_str()) {
                " [required]"
            } else {
                ""
            };
            output.stdout(format_args!(
                "    --{:<width$}  <{}>  {}{}",
                name,
                type_str,
                desc,
                req_marker,
                width = max_name,
            ));
        }
        output.stdout(format_args!(""));
    }

    output.stdout(format_args!("USAGE:"));
    let example_args: String = required.iter().map(|r| format!(" --{r} <value>")).collect();
    output.stdout(format_args!(
        "    mcpctl {server} {}{}",
        tool.name, example_args
    ));
}

fn schema_type_label(schema: &serde_json::Value) -> &str {
    match schema.get("type").and_then(|v| v.as_str()) {
        Some("string") => "STRING",
        Some("integer") => "INT",
        Some("number") => "NUMBER",
        Some("boolean") => "BOOL",
        Some("array") => "JSON",
        Some("object") => "JSON",
        _ => "VALUE",
    }
}

async fn call_tool(
    client: &impl BridgeBackend,
    server: &str,
    tool: &str,
    args: &[String],
    output: &mut CommandOutput,
) -> Result<bool, crate::client::Error> {
    let detail = bounded_response(client.server_tools(server).await?)?;
    let tool_info = detail.tools.iter().find(|t| t.name == tool);

    let Some(tool_info) = tool_info else {
        output.stderr(format_args!("Unknown tool '{tool}' on server '{server}'."));
        output.stderr(format_args!("Run: mcpctl {server} --help"));
        return Ok(false);
    };

    let arguments = parse_tool_args(tool_info, args)?;

    let result = bounded_response(client.call_tool(server, tool, arguments).await?)?;
    if result.is_error {
        output.stderr(format_args!("{}", result.content));
        Ok(false)
    } else {
        output.stdout(format_args!("{}", result.content));
        Ok(true)
    }
}

fn parse_tool_args(
    tool_info: &BridgeToolInfo,
    args: &[String],
) -> Result<serde_json::Value, crate::client::Error> {
    let props = tool_info
        .input_schema
        .get("properties")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let required_fields: Vec<String> = tool_info
        .input_schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut cmd = Command::new("mcpctl")
        .disable_help_flag(true)
        .no_binary_name(true);

    let param_names: Vec<String> = props.keys().cloned().collect();

    for (name, schema) in &props {
        let type_str = schema
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("string");
        let desc = schema
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let is_required = required_fields.iter().any(|r| r == name);

        let arg = match type_str {
            "boolean" => Arg::new(name.clone())
                .long(name.clone())
                .help(desc)
                .num_args(0..=1)
                .default_missing_value("true")
                .required(is_required),
            _ => Arg::new(name.clone())
                .long(name.clone())
                .help(desc)
                .required(is_required),
        };
        cmd = cmd.arg(arg);
    }

    let flag_args: Vec<&str> = args
        .iter()
        .skip(1)
        .skip_while(|a| !a.starts_with('-'))
        .map(|s| s.as_str())
        .collect();

    let matches = cmd
        .try_get_matches_from(&flag_args)
        .map_err(|e| crate::client::Error::Api {
            status: 0,
            body: e.to_string(),
        })?;

    let mut arguments = serde_json::Map::new();
    for name in &param_names {
        let schema = &props[name];
        let type_str = schema
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("string");

        match type_str {
            "boolean" => {
                if let Some(val) = matches.get_one::<String>(name.as_str()) {
                    let b = val == "true" || val == "1";
                    arguments.insert(name.clone(), serde_json::Value::Bool(b));
                }
            }
            "integer" => {
                if let Some(val) = matches.get_one::<String>(name.as_str()) {
                    if let Ok(n) = val.parse::<i64>() {
                        arguments.insert(name.clone(), serde_json::json!(n));
                    } else {
                        arguments.insert(name.clone(), serde_json::Value::String(val.clone()));
                    }
                }
            }
            "number" => {
                if let Some(val) = matches.get_one::<String>(name.as_str()) {
                    if let Ok(n) = val.parse::<f64>() {
                        arguments.insert(
                            name.clone(),
                            serde_json::Number::from_f64(n)
                                .map(serde_json::Value::Number)
                                .unwrap_or(serde_json::Value::String(val.clone())),
                        );
                    } else {
                        arguments.insert(name.clone(), serde_json::Value::String(val.clone()));
                    }
                }
            }
            "object" | "array" => {
                if let Some(val) = matches.get_one::<String>(name.as_str()) {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(val) {
                        arguments.insert(name.clone(), parsed);
                    } else {
                        arguments.insert(name.clone(), serde_json::Value::String(val.clone()));
                    }
                }
            }
            _ => {
                if let Some(val) = matches.get_one::<String>(name.as_str()) {
                    arguments.insert(name.clone(), serde_json::Value::String(val.clone()));
                }
            }
        }
    }

    Ok(serde_json::Value::Object(arguments))
}

// The direct backend needs the same response budget as the HTTP client.
// Count serialized bytes without allocating another copy of the response.
fn bounded_response<T: serde::Serialize>(response: T) -> Result<T, crate::Error> {
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("response limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(4 * 1024 * 1024), &response)
        .map_err(|_| crate::Error::Invalid("Bridge response exceeds the 4 MiB limit"))?;
    Ok(response)
}
