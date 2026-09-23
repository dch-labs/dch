//! The LSP tool — hover and go-to-definition through a language server.

use std::future::Future;
use std::pin::Pin;

use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use loopctl::tool::ToolSchema;
use lsp_types::Position;
use serde_json::Value;
use serde_json::json;
use url::Url;

use crate::context::RunnerContext;
use crate::context::require_cwd;
use crate::context::runner_ctx;
use crate::input::get_u64;
use crate::lsp::client::SpawnError;
use crate::lsp::get_server_for_file;
use crate::lsp::pool::evict_root;
use crate::lsp::pool::pooled_client;
use crate::util::is_url;
use crate::util::resolve_path;

/// Language Server Protocol operations for code intelligence.
///
/// Provides hover and go-to-definition for Rust files through a
/// rust-analyzer process kept alive per project root; other file types
/// are not configured and fail with a clear error. Queries never mutate
/// files, and calls against different roots are independent, so the tool
/// is read-only and safe to run concurrently.
pub struct LspTool;

impl Tool for LspTool {
    fn name(&self) -> &'static str {
        "LSP"
    }

    fn description(&self) -> &'static str {
        "Language Server Protocol for code intelligence. Provides \
         go-to-definition and hover information for Rust files via \
         rust-analyzer."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "operation": {
                        "type": "string",
                        "enum": ["goToDefinition", "hover"],
                        "description": "The LSP operation to perform"
                    },
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to analyze"
                    },
                    "line": {
                        "type": "integer",
                        "description": "Line number (1-indexed)",
                        "minimum": 1
                    },
                    "character": {
                        "type": "integer",
                        "description": "Character position within the line, counted in Unicode characters (1-indexed)",
                        "minimum": 1
                    }
                },
                "required": ["operation", "file_path", "line", "character"]
            }),
        }
    }

    fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let runner = runner_ctx(ctx).cloned();
        Box::pin(self.call_inner(input, runner))
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    fn system_prompt(&self) -> Option<String> {
        Some(
            "Use the LSP tool for precise code navigation: goToDefinition to resolve \
              a symbol's definition, hover to inspect its type and documentation. \
              Only Rust files (rust-analyzer) are supported. Positions are \
              1-indexed; characters are counted in Unicode characters."
                .to_string(),
        )
    }
}

impl LspTool {
    /// Body of [`Tool::call`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::InvalidInput`] for missing or malformed
    /// fields, a URL `file_path`, a zero position, or a path escaping
    /// the workspace under the contained policy;
    /// [`ToolError::Execution`] when no server is configured for the
    /// file's extension or the server exchange fails — the failed client
    /// is evicted from the pool so the next call cold-starts a fresh one.
    /// A missing file, missing server binary, or unknown operation is a
    /// soft `is_error` result instead.
    async fn call_inner(
        &self,
        input: Value,
        runner: Option<RunnerContext>,
    ) -> Result<ToolOutput, ToolError> {
        let operation = input
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("Missing 'operation'".to_string()))?
            .to_string();
        match operation.as_str() {
            "hover" | "goToDefinition" => {}
            other => {
                return Ok(ToolOutput::error_text(format!(
                    "Unknown operation: {other}"
                )));
            }
        }
        let file_path_str = input
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("Missing 'file_path'".to_string()))?
            .to_string();
        if is_url(&file_path_str) {
            return Err(ToolError::InvalidInput(
                "URLs are not supported by the LSP tool. LSP requires local files.".to_string(),
            ));
        }
        let line = parse_position(&input, "line")?;
        let character = parse_position(&input, "character")?;

        let cwd = require_cwd(runner.clone())?;
        let policy = runner
            .as_ref()
            .map_or(crate::util::ResolvePolicy::default(), |rc| {
                rc.resolve_policy
            });
        let full_path = resolve_path(&file_path_str, &cwd, policy)?;
        if !full_path.exists() {
            return Ok(ToolOutput::error_text(format!(
                "File not found: {}",
                full_path.display()
            )));
        }

        let server_config = get_server_for_file(&full_path).ok_or_else(|| {
            ToolError::Execution(format!(
                "No LSP server configured for file extension: {:?}",
                full_path.extension()
            ))
        })?;

        let root_uri = Url::from_file_path(&cwd)
            .map_err(|()| ToolError::Execution("Invalid project root path".to_string()))?;
        let client = match pooled_client(&cwd, &root_uri, &server_config).await {
            Ok(client) => client,
            Err(SpawnError::BinaryMissing { command }) => {
                return Ok(ToolOutput::error_text(format!(
                    "LSP server '{command}' not found. Please install it to use LSP features."
                )));
            }
            Err(SpawnError::Failed(e)) => return Err(e),
        };

        let document_uri = Url::from_file_path(&full_path)
            .map_err(|()| ToolError::Execution("Invalid file path".to_string()))?;
        let text = tokio::fs::read_to_string(&full_path)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to read {file_path_str}: {e}")))?;
        let position = to_wire_position(&text, line, character);
        let exchange = async {
            let mut client = client.lock().await;
            client.open_document(&document_uri, &text).await?;
            let result = if operation.as_str() == "hover" {
                let hover = client.hover(&document_uri, position).await?;
                match hover {
                    Some(h) => json!({
                        "operation": "hover",
                        "file_path": file_path_str,
                        "line": line,
                        "character": character,
                        "result": extract_hover_content(h)
                    }),
                    None => json!({
                        "operation": "hover",
                        "file_path": file_path_str,
                        "line": line,
                        "character": character,
                        "result": null,
                        "message": "No hover information available at this location"
                    }),
                }
            } else {
                let definition = client.goto_definition(&document_uri, position).await?;
                match definition {
                    Some(d) => {
                        let locations: Vec<Value> = extract_goto_definition_locations(d)
                            .iter()
                            .map(|location| location_json(location, document_uri.as_str(), &text))
                            .collect();
                        json!({
                            "operation": "goToDefinition",
                            "file_path": file_path_str,
                            "line": line,
                            "character": character,
                            "result": locations
                        })
                    }
                    None => json!({
                        "operation": "goToDefinition",
                        "file_path": file_path_str,
                        "line": line,
                        "character": character,
                        "result": null,
                        "message": "No definition found at this location"
                    }),
                }
            };
            Ok(result)
        }
        .await;
        match exchange {
            Ok(result) => Ok(ToolOutput::text(result.to_string())),
            Err(e) => {
                evict_root(&cwd, &client).await;
                Err(e)
            }
        }
    }
}

/// Parse a 1-indexed position field, rejecting zero and overflow.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when the field is missing, is not
/// a non-negative integer, does not fit a `u32`, or is zero.
fn parse_position(input: &Value, field: &str) -> Result<u32, ToolError> {
    let value = get_u64(input, field)?
        .ok_or_else(|| ToolError::InvalidInput(format!("Missing '{field}'")))?;
    if value == 0 {
        return Err(ToolError::InvalidInput(format!(
            "'{field}' must be at least 1, got 0"
        )));
    }
    u32::try_from(value)
        .map_err(|_| ToolError::InvalidInput(format!("'{field}' does not fit a position: {value}")))
}

/// Convert the tool's 1-indexed line and character into a wire position.
///
/// The tool's contract counts lines and characters (Unicode scalar
/// values); the wire, under the UTF-8 encoding negotiated at initialize,
/// counts bytes within the line. The line's text comes from `text` to
/// translate the character offset. A line past the end of the document
/// passes through and a character past the end of the line clamps — the
/// server answers `null` for such positions, which the tool reports
/// as-is.
fn to_wire_position(text: &str, line: u32, character: u32) -> Position {
    let line_index = line.saturating_sub(1);
    let character_index = character.saturating_sub(1);
    let Some(line_text) = text.lines().nth(line_index as usize) else {
        return Position::new(line_index, character_index);
    };
    let char_count = line_text.chars().count();
    let wanted = usize::try_from(character_index)
        .unwrap_or(char_count)
        .min(char_count);
    let byte_offset = line_text
        .char_indices()
        .nth(wanted)
        .map_or(line_text.len(), |(offset, _)| offset);
    Position::new(line_index, u32::try_from(byte_offset).unwrap_or(u32::MAX))
}

/// One definition location as the tool's output JSON, 1-indexed.
///
/// Lines convert back with `+1`. Characters convert from the wire's
/// UTF-8 byte offsets back to character counts when the location points
/// into the queried file — its text is at hand; a location in another
/// file reports the byte offset with `+1`, since the text to convert
/// against is not.
fn location_json(location: &lsp_types::Location, queried_uri: &str, text: &str) -> Value {
    let same_file = location.uri.as_str() == queried_uri;
    json!({
        "uri": location.uri.as_str(),
        "range": {
            "start": {
                "line": location.range.start.line.saturating_add(1),
                "character": display_character(same_file, text, location.range.start)
            },
            "end": {
                "line": location.range.end.line.saturating_add(1),
                "character": display_character(same_file, text, location.range.end)
            }
        }
    })
}

/// A wire position's character as a 1-indexed character count.
///
/// Converts against `text` when the location is the queried file;
/// otherwise the byte offset passes through with `+1`.
fn display_character(same_file: bool, text: &str, position: lsp_types::Position) -> u32 {
    if same_file && let Some(line_text) = text.lines().nth(position.line as usize) {
        let byte = usize::try_from(position.character).unwrap_or(line_text.len());
        let chars = line_text
            .char_indices()
            .take_while(|(offset, _)| *offset < byte)
            .count();
        return u32::try_from(chars).unwrap_or(u32::MAX).saturating_add(1);
    }
    position.character.saturating_add(1)
}

/// Flatten a hover response into displayable text.
///
/// Covers every `HoverContents` shape a server may answer with: markdown
/// markup verbatim, marked strings as text or fenced code, and arrays
/// joined with blank lines.
fn extract_hover_content(hover: lsp_types::Hover) -> String {
    use lsp_types::HoverContents;
    match hover.contents {
        HoverContents::Markup(markup) => markup.value,
        HoverContents::Scalar(marked_string) => marked_string_text(marked_string),
        HoverContents::Array(contents) => contents
            .into_iter()
            .map(marked_string_text)
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

/// One marked string's text, fencing language-tagged content.
///
/// A bare string returns its text verbatim; a language-tagged string is
/// wrapped in a fenced code block so the language tag survives into the
/// tool's markdown-rendered output.
fn marked_string_text(marked: lsp_types::MarkedString) -> String {
    use lsp_types::MarkedString;
    match marked {
        MarkedString::String(text) => text,
        MarkedString::LanguageString(lang) => {
            format!("```{}\n{}\n```", lang.language, lang.value)
        }
    }
}

/// Flatten a definition response into plain locations.
///
/// `LocationLink` responses carry their target range outside the
/// `Location` shape; they map to the target URI with a default range,
/// which preserves where the definition lives at the cost of the span.
fn extract_goto_definition_locations(
    response: lsp_types::GotoDefinitionResponse,
) -> Vec<lsp_types::Location> {
    use lsp_types::GotoDefinitionResponse;
    match response {
        GotoDefinitionResponse::Scalar(location) => vec![location],
        GotoDefinitionResponse::Array(locations) => locations,
        GotoDefinitionResponse::Link(links) => links
            .into_iter()
            .map(|link| lsp_types::Location {
                uri: link.target_uri,
                range: lsp_types::Range::default(),
            })
            .collect(),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing,
    clippy::redundant_closure_for_method_calls
)]
mod tests {
    use super::*;
    use crate::context::RunnerContext;
    use crate::lsp::pool::SPAWN_GATE;
    use loopctl::tool::ToolContext;
    use serde_json::json;

    fn ctx_in(cwd: &std::path::Path) -> ToolContext {
        let mut ctx = ToolContext {
            cwd: cwd.to_string_lossy().into_owned(),
            ..ToolContext::default()
        };
        ctx.set_extension(RunnerContext::new(cwd.to_path_buf()));
        ctx
    }

    fn lsp_input(file_path: &str, line: u64, character: u64) -> Value {
        json!({
            "operation": "hover",
            "file_path": file_path,
            "line": line,
            "character": character
        })
    }

    #[test]
    fn tool_metadata_matches_the_spec() {
        assert_eq!(LspTool.name(), "LSP");
        assert!(LspTool.is_read_only());
        assert!(LspTool.is_concurrency_safe());
        assert!(LspTool.system_prompt().is_some());
        let reg = crate::registry::builtin_registry();
        assert!(reg.get("LSP").is_some(), "registered");
    }

    #[test]
    fn schema_shape_is_snake_case_v1() {
        let schema = LspTool.schema();
        let input = schema.input_schema;
        let properties = input
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        assert_eq!(properties.len(), 4, "exactly four properties");
        for key in ["operation", "file_path", "line", "character"] {
            assert!(properties.contains_key(key), "{key} present");
        }
        assert!(
            !properties.contains_key("filePath"),
            "the input field is snake_case, not camelCase"
        );
        let required = input.get("required").and_then(Value::as_array).unwrap();
        assert_eq!(required.len(), 4);
        let operations = input
            .pointer("/properties/operation/enum")
            .and_then(Value::as_array)
            .unwrap();
        assert!(operations.contains(&json!("goToDefinition")));
        assert!(operations.contains(&json!("hover")));
        assert_eq!(
            input
                .pointer("/properties/line/minimum")
                .and_then(Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn extract_hover_content_variants() {
        use lsp_types::Hover;
        use lsp_types::HoverContents;
        use lsp_types::LanguageString;
        use lsp_types::MarkedString;
        use lsp_types::MarkupContent;
        use lsp_types::MarkupKind;

        let markup = Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: "Test hover text".to_string(),
            }),
            range: None,
        };
        assert_eq!(extract_hover_content(markup), "Test hover text");

        let scalar = Hover {
            contents: HoverContents::Scalar(MarkedString::String("Simple text".to_string())),
            range: None,
        };
        assert_eq!(extract_hover_content(scalar), "Simple text");

        let language = Hover {
            contents: HoverContents::Scalar(MarkedString::LanguageString(LanguageString {
                language: "rust".to_string(),
                value: "fn main() {}".to_string(),
            })),
            range: None,
        };
        assert_eq!(
            extract_hover_content(language),
            "```rust\nfn main() {}\n```"
        );

        let array = Hover {
            contents: HoverContents::Array(vec![
                MarkedString::String("one".to_string()),
                MarkedString::String("two".to_string()),
            ]),
            range: None,
        };
        assert_eq!(extract_hover_content(array), "one\n\ntwo");
    }

    #[test]
    fn extract_goto_definition_variants() {
        use lsp_types::GotoDefinitionResponse;
        use lsp_types::Location;
        use lsp_types::LocationLink;
        use lsp_types::Range;
        use lsp_types::Uri;
        use std::str::FromStr;

        let uri = Uri::from_str("file:///test/file.rs").unwrap();
        let scalar = GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range: Range::default(),
        });
        assert_eq!(extract_goto_definition_locations(scalar).len(), 1);

        let array = GotoDefinitionResponse::Array(vec![
            Location {
                uri: uri.clone(),
                range: Range::default(),
            },
            Location {
                uri: uri.clone(),
                range: Range::default(),
            },
        ]);
        assert_eq!(extract_goto_definition_locations(array).len(), 2);

        let link = GotoDefinitionResponse::Link(vec![LocationLink {
            origin_selection_range: None,
            target_uri: uri,
            target_range: Range::default(),
            target_selection_range: Range::default(),
        }]);
        let locations = extract_goto_definition_locations(link);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].range, Range::default());
    }

    #[test]
    fn wire_positions_translate_characters_into_utf8_bytes() {
        let text = "let s = \"héllo\";\nlet x = 1;\n";
        assert_eq!(
            to_wire_position(text, 1, 11),
            Position::new(0, 10),
            "the é at character 11 sits after 10 single-byte characters"
        );
        assert_eq!(
            to_wire_position(text, 1, 13),
            Position::new(0, 13),
            "two positions past é shift by its extra byte"
        );
        assert_eq!(
            to_wire_position(text, 1, 1),
            Position::new(0, 0),
            "line/character 1 is the origin"
        );
    }

    #[test]
    fn wire_positions_clamp_and_pass_through_edges() {
        let text = "let x = 1;\n";
        assert_eq!(
            to_wire_position(text, 1, 99),
            Position::new(0, 10),
            "a character past the line's end clamps to its length"
        );
        assert_eq!(
            to_wire_position(text, 9, 3),
            Position::new(8, 2),
            "a line past the document passes through for the server to answer null"
        );
    }

    #[test]
    fn same_file_locations_report_character_counts() {
        use lsp_types::Range;
        use lsp_types::Uri;
        use std::str::FromStr;

        let text = "let s = \"héllo\";\n";
        let uri = Uri::from_str("file:///work/a.rs").unwrap();
        let location = lsp_types::Location {
            uri,
            range: Range::new(Position::new(0, 14), Position::new(0, 15)),
        };
        let same = location_json(&location, "file:///work/a.rs", text);
        assert_eq!(same.pointer("/range/start/character"), Some(&json!(14)));
        assert_eq!(same.pointer("/range/end/character"), Some(&json!(15)));

        let foreign = location_json(&location, "file:///elsewhere.rs", text);
        assert_eq!(foreign.pointer("/range/start/character"), Some(&json!(15)));
        assert_eq!(foreign.pointer("/uri"), Some(&json!("file:///work/a.rs")));
    }

    #[tokio::test]
    async fn missing_input_is_a_structural_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = LspTool
            .call(json!({ "operation": "hover" }), &ctx_in(tmp.path()))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
    }

    #[tokio::test]
    async fn url_file_paths_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let err = LspTool
            .call(
                lsp_input("https://example.com/x.rs", 1, 1),
                &ctx_in(tmp.path()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("URLs are not supported")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn zero_positions_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        for field in ["line", "character"] {
            let mut input = lsp_input("src/lib.rs", 1, 1);
            input[field] = json!(0);
            let err = LspTool.call(input, &ctx_in(tmp.path())).await.unwrap_err();
            assert!(
                matches!(err, ToolError::InvalidInput(ref s) if s.contains(field)),
                "{field} zero rejected: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn paths_escaping_the_workspace_are_rejected() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.rs");
        std::fs::write(&secret, "pub fn x() {}\n").unwrap();

        let err = LspTool
            .call(
                lsp_input(secret.to_str().unwrap(), 1, 1),
                &ctx_in(workspace.path()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("escapes")),
            "contained policy rejects the escape: {err:?}"
        );
    }

    #[tokio::test]
    async fn unsupported_extension_is_an_execution_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("script.py"), "print('hi')\n").unwrap();
        let err = LspTool
            .call(lsp_input("script.py", 1, 1), &ctx_in(tmp.path()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(ref s) if s.contains("No LSP server configured")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn missing_file_is_a_soft_error() {
        let tmp = tempfile::tempdir().unwrap();
        let out = LspTool
            .call(lsp_input("nope.rs", 1, 1), &ctx_in(tmp.path()))
            .await
            .unwrap();
        assert!(out.is_error, "soft error");
        assert!(out.text_content().contains("File not found"));
    }

    #[tokio::test]
    async fn an_unknown_operation_is_a_soft_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut input = lsp_input("src/lib.rs", 1, 1);
        input["operation"] = json!("references");
        let out = LspTool.call(input, &ctx_in(tmp.path())).await.unwrap();
        assert!(out.is_error, "soft error: {}", out.text_content());
        assert!(
            out.text_content().contains("Unknown operation: references"),
            "{}",
            out.text_content()
        );
    }

    /// Whether a rust-analyzer binary is reachable — the live tests skip
    /// (pass vacuously) where it is not installed.
    fn rust_analyzer_available() -> bool {
        std::process::Command::new("rust-analyzer")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// A minimal cargo crate whose `add` definition the live tests
    /// navigate.
    fn fixture_crate() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"lsp_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn uses_add() -> i32 {\n    add(1, 2)\n}\n",
        )
        .unwrap();
        dir
    }

    /// Drive one call per attempt until `ready` accepts the output or the
    /// attempts run out.
    ///
    /// The server answers position queries from its in-memory documents
    /// immediately, but full resolution arrives with the workspace load —
    /// a background cargo fetch whose speed depends on machine load. Live
    /// tests therefore poll the call instead of trusting the first answer.
    async fn eventually<F, Fut>(mut attempt: F, ready: fn(&str) -> bool)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = String>,
    {
        for _ in 0..20 {
            let text = attempt().await;
            if ready(&text) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        panic!("condition never became ready");
    }

    #[tokio::test]
    async fn hover_returns_markup_on_a_real_rust_file() {
        if !rust_analyzer_available() {
            return;
        }
        let _guard = SPAWN_GATE.lock().await;
        let dir = fixture_crate();
        let ctx = ctx_in(dir.path());
        // `pub fn add` sits on line 1; `add` starts at character 8.
        eventually(
            || async {
                LspTool
                    .call(lsp_input("src/lib.rs", 1, 8), &ctx)
                    .await
                    .unwrap()
                    .text_content()
            },
            |text| {
                text.contains("\"operation\":\"hover\"")
                    && text.contains("fn add")
                    && text.contains("i32")
            },
        )
        .await;
    }

    #[tokio::test]
    async fn goto_definition_resolves_the_symbol() {
        if !rust_analyzer_available() {
            return;
        }
        let _guard = SPAWN_GATE.lock().await;
        let dir = fixture_crate();
        // `add(1, 2)` on line 5; the call starts at character 5.
        let mut input = lsp_input("src/lib.rs", 5, 5);
        input["operation"] = json!("goToDefinition");
        let out = LspTool.call(input, &ctx_in(dir.path())).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(text.contains("lib.rs"), "definition in lib.rs: {text}");
        assert!(
            text.contains("\"result\":[") || text.contains("\"result\": ["),
            "non-empty locations: {text}"
        );
    }

    #[tokio::test]
    async fn the_pool_reuses_one_server_across_calls() {
        if !rust_analyzer_available() {
            return;
        }
        let _guard = SPAWN_GATE.lock().await;
        let dir = fixture_crate();
        let ctx = ctx_in(dir.path());
        let before = crate::lsp::pool::pool_spawn_count();
        let first = LspTool
            .call(lsp_input("src/lib.rs", 1, 8), &ctx)
            .await
            .unwrap();
        assert!(!first.is_error);
        let after_first = crate::lsp::pool::pool_spawn_count();
        let second = LspTool
            .call(lsp_input("src/lib.rs", 1, 8), &ctx)
            .await
            .unwrap();
        assert!(!second.is_error);
        assert_eq!(after_first, before + 1, "first call spawned one server");
        assert_eq!(
            crate::lsp::pool::pool_spawn_count(),
            after_first,
            "second call reused the pooled server"
        );
    }

    #[tokio::test]
    async fn hover_on_an_empty_location_returns_null_with_a_message() {
        if !rust_analyzer_available() {
            return;
        }
        let _guard = SPAWN_GATE.lock().await;
        let dir = fixture_crate();
        // Line 2 is `    a + b` — column 1 lands on whitespace.
        let out = LspTool
            .call(lsp_input("src/lib.rs", 2, 1), &ctx_in(dir.path()))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(text.contains("\"result\":null"), "{text}");
        assert!(text.contains("No hover information available"), "{text}");
    }
}
