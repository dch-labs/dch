//! The LSP tool — code intelligence through a language server.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
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
use crate::lsp::client::RequestError;
use crate::lsp::client::SpawnError;
use crate::lsp::get_server_for_file;
use crate::lsp::pool::evict_root;
use crate::lsp::pool::pooled_client;
use crate::util::ResolvePolicy;
use crate::util::is_file_url;
use crate::util::is_url;
use crate::util::resolve_path;

/// The cap on a location-list result.
///
/// A hot symbol's reference list can reach hundreds of rows; every row
/// is context the model pays for, so the envelope keeps the first
/// [`MAX_LOCATION_RESULTS`] and reports the rest as an `omitted` count
/// for the model to narrow against.
const MAX_LOCATION_RESULTS: usize = 50;

/// The cap on a documentSymbol outline.
///
/// An outline row is cheaper than a location row, but a large file's
/// full outline is still context-hostile; the same cut-and-report
/// contract applies at a wider bound.
const MAX_DOCUMENT_SYMBOLS: usize = 200;

/// Language Server Protocol operations for code intelligence.
///
/// Provides hover, go-to-definition, references, implementations, and
/// document symbols for Rust files through a rust-analyzer process kept
/// alive per project root; other file types are not configured and fail
/// with a clear error. Queries never mutate files, and calls against
/// different roots are independent, so the tool is read-only and safe
/// to run concurrently. Result rows may name paths outside the
/// workspace — the sysroot and registry behind library items — while
/// the resolve policy still gates reading them: an out-of-policy
/// target reports marked byte offsets and never its contents.
pub struct LspTool;

impl Tool for LspTool {
    fn name(&self) -> &'static str {
        "LSP"
    }

    fn description(&self) -> &'static str {
        "Language Server Protocol for code intelligence. Provides \
         go-to-definition, hover, references, implementations, and \
         document symbols for Rust files via rust-analyzer."
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
                        "enum": [
                            "goToDefinition",
                            "hover",
                            "references",
                            "implementations",
                            "documentSymbol"
                        ],
                        "description": "The LSP operation to perform"
                    },
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to analyze"
                    },
                    "line": {
                        "type": "integer",
                        "description": "Line number (1-indexed); required for goToDefinition, hover, references, and implementations, ignored by documentSymbol",
                        "minimum": 1
                    },
                    "character": {
                        "type": "integer",
                        "description": "Character position within the line, counted in Unicode characters (1-indexed); required for goToDefinition, hover, references, and implementations, ignored by documentSymbol",
                        "minimum": 1
                    },
                    "include_declaration": {
                        "type": "boolean",
                        "description": "For references only: include the symbol's declaration site in the results. Defaults to true; ignored by other operations"
                    }
                },
                "required": ["operation", "file_path"]
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
            "Use the LSP tool for precise code navigation: goToDefinition \
              to resolve a symbol's definition, hover to inspect its type \
              and documentation, references to list a symbol's usages \
              (semantic hits — prefer it over Grep for call sites), \
              implementations to list a trait or type's implementors, \
              documentSymbol to outline a file's symbols and orient in a \
              large file faster than reading it. Only Rust files \
              (rust-analyzer) are supported. Positions are 1-indexed; \
              characters are counted in Unicode characters."
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
    /// fields — a missing operation, a URL `file_path`, a missing or
    /// zero position on a position-bearing operation, a non-boolean
    /// `include_declaration`, or a path escaping the workspace under
    /// the contained policy; [`ToolError::Execution`] when no server is
    /// configured for the file's extension or the server exchange
    /// fails — only a transport failure evicts the pooled client (a
    /// JSON-RPC error reply means a healthy server), so the next call
    /// cold-starts solely when the stream is suspect. A missing file,
    /// missing server binary, or unknown operation is a soft `is_error`
    /// result instead.
    async fn call_inner(
        &self,
        input: Value,
        runner: Option<RunnerContext>,
    ) -> Result<ToolOutput, ToolError> {
        let operation = match parse_operation(&input) {
            Ok(operation) => operation,
            Err(OperationError::Unknown(name)) => {
                return Ok(ToolOutput::error_text(format!("Unknown operation: {name}")));
            }
            Err(OperationError::Structural(error)) => return Err(error),
        };
        let file_path_str = input
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("Missing 'file_path'".to_string()))?
            .to_string();
        if is_file_url(&file_path_str) {
            return Err(ToolError::InvalidInput(
                "file:// URLs are not supported by the LSP tool. Pass a filesystem path instead."
                    .to_string(),
            ));
        }
        if is_url(&file_path_str) {
            return Err(ToolError::InvalidInput(
                "URLs are not supported by the LSP tool. LSP requires local files.".to_string(),
            ));
        }

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
        let exchange = async {
            let mut client = client.lock().await;
            client.open_document(&document_uri, &text).await?;
            let result = match operation {
                Operation::Hover { line, character } => {
                    let position = to_wire_position(&text, line, character);
                    let hover = client.hover(&document_uri, position).await?;
                    hover_envelope(&file_path_str, line, character, hover)
                }
                Operation::GoToDefinition { line, character } => {
                    let position = to_wire_position(&text, line, character);
                    let response = client.goto_definition(&document_uri, position).await?;
                    location_envelope(
                        LocationQuery {
                            operation: "goToDefinition",
                            file_path: &file_path_str,
                            line,
                            character,
                            empty_message: "No definition found at this location",
                            found: response.map(extract_goto_definition_locations),
                        },
                        &document_uri,
                        &text,
                        &cwd,
                        policy,
                    )
                    .await
                }
                Operation::References {
                    line,
                    character,
                    include_declaration,
                } => {
                    let position = to_wire_position(&text, line, character);
                    let found = client
                        .references(&document_uri, position, include_declaration)
                        .await?;
                    location_envelope(
                        LocationQuery {
                            operation: "references",
                            file_path: &file_path_str,
                            line,
                            character,
                            empty_message: "No references found at this location",
                            found,
                        },
                        &document_uri,
                        &text,
                        &cwd,
                        policy,
                    )
                    .await
                }
                Operation::Implementations { line, character } => {
                    let position = to_wire_position(&text, line, character);
                    let response = client.implementations(&document_uri, position).await?;
                    location_envelope(
                        LocationQuery {
                            operation: "implementations",
                            file_path: &file_path_str,
                            line,
                            character,
                            empty_message: "No implementations found at this location",
                            found: response.map(extract_goto_definition_locations),
                        },
                        &document_uri,
                        &text,
                        &cwd,
                        policy,
                    )
                    .await
                }
                Operation::DocumentSymbol => {
                    let response = client.document_symbol(&document_uri).await?;
                    document_symbol_envelope(&file_path_str, response, &text)
                }
            };
            Ok(result)
        }
        .await;
        match exchange {
            Ok(result) => Ok(ToolOutput::text(result.to_string())),
            Err(RequestError::Transport(e)) => {
                evict_root(&cwd, &client).await;
                Err(e)
            }
            Err(RequestError::Server(e)) => Err(e),
        }
    }
}

/// The operation named in the input, with its per-operation fields.
///
/// Parsing validates the schema's per-operation contract once — which
/// operations require a position, what `references` reads — so dispatch
/// matches a total enum instead of re-checking strings, and the
/// contract has a single unit-testable seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    /// Hover markup for the symbol at a position.
    ///
    /// The query the server answers with type and documentation
    /// markup; an empty answer keeps the envelope's standing message
    /// instead of an empty list.
    Hover {
        /// The 1-indexed line the hover is requested at.
        ///
        /// Carried in the tool's input unit; the query converts it to
        /// the wire's 0-indexed form in [`to_wire_position`].
        line: u32,

        /// The 1-indexed character the hover is requested at.
        ///
        /// Counted in Unicode characters per the tool's advertised
        /// contract; the wire conversion shifts it to a byte offset
        /// within the line.
        character: u32,
    },

    /// The definition of the symbol at a position.
    ///
    /// Answered as a location list, so the envelope flows through
    /// [`location_envelope`] with the shared cap and unit conversion.
    GoToDefinition {
        /// The 1-indexed line of the symbol being resolved.
        ///
        /// Carried in the tool's input unit; the query converts it to
        /// the wire's 0-indexed form in [`to_wire_position`].
        line: u32,

        /// The 1-indexed character of the symbol being resolved.
        ///
        /// Counted in Unicode characters per the tool's advertised
        /// contract; the wire conversion shifts it to a byte offset
        /// within the line.
        character: u32,
    },

    /// The usages of the symbol at a position.
    ///
    /// The semantic alternative to word-shaped Grep matches; the
    /// envelope is the same location list [`Operation::GoToDefinition`]
    /// produces, under the same cap.
    References {
        /// The 1-indexed line of the symbol whose usages are wanted.
        ///
        /// Carried in the tool's input unit; the query converts it to
        /// the wire's 0-indexed form in [`to_wire_position`].
        line: u32,

        /// The 1-indexed character of the symbol whose usages are
        /// wanted.
        ///
        /// Counted in Unicode characters per the tool's advertised
        /// contract; the wire conversion shifts it to a byte offset
        /// within the line.
        character: u32,

        /// Whether the declaration site joins the usage list.
        ///
        /// Absent input means `true` — the declaration is itself an
        /// edit-relevant usage — and the value travels to the server
        /// as the reference context's flag.
        include_declaration: bool,
    },

    /// The implementors of the trait or type at a position.
    ///
    /// Answered as a location list, so the envelope flows through
    /// [`location_envelope`] with the shared cap and unit conversion.
    Implementations {
        /// The 1-indexed line of the trait or type.
        ///
        /// Carried in the tool's input unit; the query converts it to
        /// the wire's 0-indexed form in [`to_wire_position`].
        line: u32,

        /// The 1-indexed character of the trait or type.
        ///
        /// Counted in Unicode characters per the tool's advertised
        /// contract; the wire conversion shifts it to a byte offset
        /// within the line.
        character: u32,
    },

    /// The file's symbol outline — a file-level query carrying no
    /// position.
    ///
    /// The outline normalizes from either wire shape the server
    /// answers with; see [`normalize_symbols`].
    DocumentSymbol,
}

/// Why parsing the operation failed.
///
/// The two failures surface differently: a structural problem rejects
/// the call, while an unknown operation name is the tool's soft error —
/// the call ran, and the answer is that no such operation exists.
#[derive(Debug)]
enum OperationError {
    /// A malformed field — surfaced as [`ToolError::InvalidInput`].
    ///
    /// A missing operation, a missing or zero position on a
    /// position-bearing operation, or a present non-boolean
    /// `include_declaration`; the call is rejected before any file or
    /// server is touched.
    Structural(ToolError),

    /// A name outside the schema's enum — surfaced as a soft error.
    ///
    /// Carries the name as sent so the reply can quote it back; the
    /// answer is a tool output rather than a rejected call, keeping
    /// the model in the conversation.
    Unknown(String),
}

/// Parse the operation and its per-operation fields from `input`.
///
/// `documentSymbol` is a file-level query and takes no position; every
/// other operation requires both position fields, and `references`
/// additionally reads its declaration toggle. The name classifies
/// before any other field is read, so an unknown operation reports
/// itself as the soft error even when the call omits the position a
/// known operation would need.
///
/// # Errors
///
/// Returns [`OperationError::Structural`] for a missing operation, a
/// missing or zero position on a position-bearing operation, or a
/// present non-boolean `include_declaration`;
/// [`OperationError::Unknown`] for a name outside the schema's enum.
fn parse_operation(input: &Value) -> Result<Operation, OperationError> {
    let name = input
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            OperationError::Structural(ToolError::InvalidInput("Missing 'operation'".to_string()))
        })?;
    match name {
        "documentSymbol" => Ok(Operation::DocumentSymbol),
        "hover" => Ok(Operation::Hover {
            line: operation_position(input, "line")?,
            character: operation_position(input, "character")?,
        }),
        "goToDefinition" => Ok(Operation::GoToDefinition {
            line: operation_position(input, "line")?,
            character: operation_position(input, "character")?,
        }),
        "references" => Ok(Operation::References {
            line: operation_position(input, "line")?,
            character: operation_position(input, "character")?,
            include_declaration: parse_include_declaration(input)
                .map_err(OperationError::Structural)?,
        }),
        "implementations" => Ok(Operation::Implementations {
            line: operation_position(input, "line")?,
            character: operation_position(input, "character")?,
        }),
        _ => Err(OperationError::Unknown(name.to_string())),
    }
}

/// One position field of a position-bearing operation, wrapped for
/// [`parse_operation`]'s error type.
///
/// A thin adapter over [`parse_position`] so each operation arm reads
/// its fields with a single `?` instead of repeating the error
/// conversion.
///
/// # Errors
///
/// Returns [`OperationError::Structural`] when the field is missing,
/// malformed, or zero.
fn operation_position(input: &Value, field: &str) -> Result<u32, OperationError> {
    parse_position(input, field).map_err(OperationError::Structural)
}

/// The `references` declaration toggle.
///
/// Absent means `true` — the declaration site is itself an edit-relevant
/// usage, so it joins the list unless the caller opts out. A present
/// non-boolean is malformed input rather than a silent default.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] for a present non-boolean value.
fn parse_include_declaration(input: &Value) -> Result<bool, ToolError> {
    match input.get("include_declaration") {
        None => Ok(true),
        Some(value) => value.as_bool().ok_or_else(|| {
            ToolError::InvalidInput("'include_declaration' must be a boolean".to_string())
        }),
    }
}

/// The inputs a location-list envelope needs beyond the locations.
///
/// Bundled so the shared formatter keeps a flat parameter list: the
/// operation identity and echoed position, the empty-answer message,
/// and the found locations.
struct LocationQuery<'a> {
    /// The wire operation name echoed into the envelope.
    ///
    /// `goToDefinition`, `references`, or `implementations`; the model
    /// reads it to know which question the rows answer.
    operation: &'a str,

    /// The file path as the caller wrote it, echoed.
    ///
    /// Unresolved and unnormalized on purpose — the echo mirrors the
    /// input so the model can match it against what it sent.
    file_path: &'a str,

    /// The echoed 1-indexed line.
    ///
    /// The line the query asked about, in the input's unit; the rows'
    /// own ranges convert independently of it.
    line: u32,

    /// The echoed 1-indexed character.
    ///
    /// The other half of the queried position, counted in Unicode
    /// characters per the tool's contract.
    character: u32,

    /// The message reported when the server found nothing.
    ///
    /// Names what was absent — definitions, references, or
    /// implementations — so an empty answer stays actionable.
    empty_message: &'a str,

    /// The found locations, or `None` when the server had no answer.
    ///
    /// `Some` with an empty list and `None` stay distinct: the former
    /// renders as an empty result, the latter takes the message path.
    found: Option<Vec<lsp_types::Location>>,
}

/// The envelope for a location-list operation.
///
/// Rows flow through [`location_json`] with each location's file text —
/// the queried file's, or a foreign file's loaded under the resolve
/// policy — so the character-unit contract holds for every row. The
/// list caps at [`MAX_LOCATION_RESULTS`] rows before foreign texts
/// load — rows past the cap never render, so their files are never
/// read — and a cut reports the dropped count as `omitted`.
async fn location_envelope(
    query: LocationQuery<'_>,
    queried_uri: &Url,
    text: &str,
    cwd: &Path,
    policy: ResolvePolicy,
) -> Value {
    let Some(found) = query.found else {
        return json!({
            "operation": query.operation,
            "file_path": query.file_path,
            "line": query.line,
            "character": query.character,
            "result": null,
            "message": query.empty_message,
        });
    };
    let (found, omitted) = cap_rows(found, MAX_LOCATION_RESULTS);
    let foreign_texts = load_foreign_texts(&found, queried_uri.as_str(), cwd, policy).await;
    let rows: Vec<Value> = found
        .iter()
        .map(|location| {
            let location_text = if location.uri.as_str() == queried_uri.as_str() {
                Some(text)
            } else {
                foreign_texts.get(location.uri.as_str()).map(String::as_str)
            };
            location_json(location, location_text)
        })
        .collect();
    let mut envelope = json!({
        "operation": query.operation,
        "file_path": query.file_path,
        "line": query.line,
        "character": query.character,
        "result": rows,
    });
    if omitted > 0
        && let Some(object) = envelope.as_object_mut()
    {
        object.insert("omitted".to_string(), json!(omitted));
    }
    envelope
}

/// The envelope for a hover reply.
///
/// A present hover flattens through [`extract_hover_content`]; an
/// absent one reports an empty result with the standing message.
fn hover_envelope(
    file_path: &str,
    line: u32,
    character: u32,
    hover: Option<lsp_types::Hover>,
) -> Value {
    match hover {
        Some(hover) => json!({
            "operation": "hover",
            "file_path": file_path,
            "line": line,
            "character": character,
            "result": extract_hover_content(hover),
        }),
        None => json!({
            "operation": "hover",
            "file_path": file_path,
            "line": line,
            "character": character,
            "result": null,
            "message": "No hover information available at this location",
        }),
    }
}

/// Cap `rows` at `cap`, reporting how many were dropped.
///
/// At or under the cap the rows pass through untouched and nothing is
/// reported; over it, the leading rows survive and the count of the
/// rest comes back for the caller's `omitted` field. Generic over the
/// row type so location lists can bound before their foreign texts
/// load.
fn cap_rows<T>(rows: Vec<T>, cap: usize) -> (Vec<T>, usize) {
    let omitted = rows.len().saturating_sub(cap);
    if omitted == 0 {
        return (rows, 0);
    }
    (rows.into_iter().take(cap).collect(), omitted)
}

/// One outline row, normalized from either documentSymbol wire shape.
///
/// Flat and hierarchical replies produce the same rows, so downstream
/// formatting never branches on which shape the server chose.
struct OutlineSymbol {
    /// The symbol's name.
    ///
    /// The identifier the server reported, verbatim — for an impl
    /// block the server names the whole `impl … for …` head.
    name: String,

    /// The symbol's kind.
    ///
    /// The wire's numeric kind, mapped to a lowercase name at
    /// formatting time in [`symbol_kind_name`].
    kind: lsp_types::SymbolKind,

    /// Where the symbol sits in the document.
    ///
    /// The enclosing range the server reported, not a name range;
    /// converted to 1-indexed endpoints in [`symbol_row`].
    range: lsp_types::Range,

    /// The name of the symbol containing this one, when the reply
    /// carried hierarchy.
    ///
    /// A flat reply's `containerName` passed through, or the parent's
    /// name assigned while flattening a nested reply.
    container: Option<String>,
}

/// Normalize a documentSymbol reply into capped flat outline rows.
///
/// A flat reply passes through, keeping its `containerName`; a
/// hierarchical reply flattens recursively with each row's container
/// set to its parent's name. Both wire shapes produce identical rows,
/// so the envelope never depends on which one the server chose.
/// Traversal holds at most `cap` rows and counts every further symbol,
/// so an oversized reply never allocates its full outline; the
/// returned pair is the retained rows and the omitted count.
fn normalize_symbols(
    response: lsp_types::DocumentSymbolResponse,
    cap: usize,
) -> (Vec<OutlineSymbol>, usize) {
    use lsp_types::DocumentSymbolResponse;
    match response {
        DocumentSymbolResponse::Flat(infos) => {
            let omitted = infos.len().saturating_sub(cap);
            let rows = infos
                .into_iter()
                .take(cap)
                .map(|info| OutlineSymbol {
                    name: info.name,
                    kind: info.kind,
                    range: info.location.range,
                    container: info.container_name,
                })
                .collect();
            (rows, omitted)
        }
        DocumentSymbolResponse::Nested(symbols) => {
            let mut rows = Vec::new();
            let mut omitted = 0;
            flatten_symbols(symbols, None, cap, &mut rows, &mut omitted);
            (rows, omitted)
        }
    }
}

/// Flatten hierarchical symbols into `rows`, naming each row's parent.
///
/// Depth-first in document order: every symbol counts, then its
/// children do, each child's container the name of the symbol that
/// contains it. Rows accumulate only while fewer than `cap` are held;
/// symbols past the cap add to `omitted` without being built.
fn flatten_symbols(
    symbols: Vec<lsp_types::DocumentSymbol>,
    parent: Option<&str>,
    cap: usize,
    rows: &mut Vec<OutlineSymbol>,
    omitted: &mut usize,
) {
    for symbol in symbols {
        let lsp_types::DocumentSymbol {
            name,
            kind,
            range,
            children,
            ..
        } = symbol;
        if rows.len() < cap {
            rows.push(OutlineSymbol {
                container: parent.map(String::from),
                name: name.clone(),
                kind,
                range,
            });
        } else {
            *omitted = omitted.saturating_add(1);
        }
        if let Some(children) = children {
            flatten_symbols(children, Some(name.as_str()), cap, rows, omitted);
        }
    }
}

/// The envelope for a documentSymbol reply.
///
/// Rows normalize through [`normalize_symbols`] with the
/// [`MAX_DOCUMENT_SYMBOLS`] cap applied mid-traversal — an oversized
/// outline never allocates past the cap — and a cut reports the
/// dropped count as `omitted`. The position-free query carries no
/// `line`/`character` echo.
fn document_symbol_envelope(
    file_path: &str,
    response: Option<lsp_types::DocumentSymbolResponse>,
    text: &str,
) -> Value {
    let Some(response) = response else {
        return json!({
            "operation": "documentSymbol",
            "file_path": file_path,
            "result": null,
            "message": "No symbols found in this document",
        });
    };
    let (symbols, omitted) = normalize_symbols(response, MAX_DOCUMENT_SYMBOLS);
    let rows: Vec<Value> = symbols
        .iter()
        .map(|symbol| symbol_row(symbol, text))
        .collect();
    let mut envelope = json!({
        "operation": "documentSymbol",
        "file_path": file_path,
        "result": rows,
    });
    if omitted > 0
        && let Some(object) = envelope.as_object_mut()
    {
        object.insert("omitted".to_string(), json!(omitted));
    }
    envelope
}

/// One outline row as JSON.
///
/// The range converts against the queried document's own text —
/// outline rows never leave it — under the same character-unit
/// contract as location rows, with the same byte-fallback marker.
fn symbol_row(symbol: &OutlineSymbol, text: &str) -> Value {
    let (range, byte_fallback) = range_json(Some(text), symbol.range);
    let mut row = json!({
        "name": symbol.name,
        "kind": symbol_kind_name(symbol.kind),
        "range": range,
    });
    if let Some(container) = &symbol.container
        && let Some(object) = row.as_object_mut()
    {
        object.insert("container".to_string(), json!(container));
    }
    if byte_fallback && let Some(object) = row.as_object_mut() {
        object.insert("character_units".to_string(), json!("utf-8-bytes"));
    }
    row
}

/// A symbol kind's lowercase name.
///
/// The wire carries an integer; the model reads a word. Kinds outside
/// the named set — a server may send any number — report as `"symbol"`
/// rather than failing the outline.
fn symbol_kind_name(kind: lsp_types::SymbolKind) -> &'static str {
    use lsp_types::SymbolKind;
    match kind {
        SymbolKind::FILE => "file",
        SymbolKind::MODULE => "module",
        SymbolKind::NAMESPACE => "namespace",
        SymbolKind::PACKAGE => "package",
        SymbolKind::CLASS => "class",
        SymbolKind::METHOD => "method",
        SymbolKind::PROPERTY => "property",
        SymbolKind::FIELD => "field",
        SymbolKind::CONSTRUCTOR => "constructor",
        SymbolKind::ENUM => "enum",
        SymbolKind::INTERFACE => "interface",
        SymbolKind::FUNCTION => "function",
        SymbolKind::VARIABLE => "variable",
        SymbolKind::CONSTANT => "constant",
        SymbolKind::STRING => "string",
        SymbolKind::NUMBER => "number",
        SymbolKind::BOOLEAN => "boolean",
        SymbolKind::ARRAY => "array",
        SymbolKind::OBJECT => "object",
        SymbolKind::KEY => "key",
        SymbolKind::NULL => "null",
        SymbolKind::ENUM_MEMBER => "enum_member",
        SymbolKind::STRUCT => "struct",
        SymbolKind::EVENT => "event",
        SymbolKind::OPERATOR => "operator",
        SymbolKind::TYPE_PARAMETER => "type_parameter",
        _ => "symbol",
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
/// UTF-8 byte offsets to character counts against `text` — the
/// location's own file, which the caller loads. Without text (the file
/// is not local, or its path fails [`ResolvePolicy`], or the read
/// failed) both characters fall back to 1-indexed byte offsets and the
/// location carries `character_units: "utf-8-bytes"`, so a byte offset
/// is never reported under the character contract unmarked.
fn location_json(location: &lsp_types::Location, text: Option<&str>) -> Value {
    let (range, byte_fallback) = range_json(text, location.range);
    let mut value = json!({
        "uri": location.uri.as_str(),
        "range": range,
    });
    if byte_fallback && let Some(object) = value.as_object_mut() {
        object.insert("character_units".to_string(), json!("utf-8-bytes"));
    }
    value
}

/// A wire range as JSON endpoints, its characters converted.
///
/// Lines shift to 1-indexed; characters convert against `text` when it
/// holds the endpoint's line, else fall back to the shifted byte
/// offset. The returned flag says whether either endpoint fell back,
/// so the caller marks its row in whatever shape it emits.
fn range_json(text: Option<&str>, range: lsp_types::Range) -> (Value, bool) {
    let start = display_character(text, range.start);
    let end = display_character(text, range.end);
    let byte_fallback = start.is_byte_fallback() || end.is_byte_fallback();
    let value = json!({
        "start": {
            "line": range.start.line.saturating_add(1),
            "character": start.value(),
        },
        "end": {
            "line": range.end.line.saturating_add(1),
            "character": end.value(),
        }
    });
    (value, byte_fallback)
}

/// A converted character position, carrying the unit it counts.
///
/// [`CharacterPosition::Chars`] is the tool's contract — a 1-indexed
/// Unicode character count. [`CharacterPosition::Bytes`] is the fallback
/// for a location whose file text could not be loaded: the wire's UTF-8
/// byte offset shifted to 1-indexed, reported only with its marker.
#[derive(Debug, Clone, Copy)]
enum CharacterPosition {
    /// A position counted in Unicode characters.
    ///
    /// The count of Unicode scalar values before the position on its
    /// line, plus one — the tool's advertised contract. Produced
    /// whenever the location's file text was loaded, which is every row
    /// unless the fallback marker says otherwise.
    Chars(u32),

    /// A position counted in UTF-8 bytes.
    ///
    /// The wire's byte offset shifted to 1-indexed, reported only when
    /// the location's file text could not be loaded. The row carries
    /// `character_units: "utf-8-bytes"` so the differing unit is
    /// explicit rather than implied.
    Bytes(u32),
}

impl CharacterPosition {
    /// The 1-indexed number, in the position's own unit.
    ///
    /// Callers serialize the value directly; the unit travels separately
    /// via [`Self::is_byte_fallback`] so only fallback rows are marked.
    fn value(self) -> u32 {
        match self {
            Self::Chars(n) | Self::Bytes(n) => n,
        }
    }

    /// Whether the value counts bytes rather than characters.
    ///
    /// Names the [`CharacterPosition::Bytes`] fallback so callers mark
    /// exactly the rows that need it.
    fn is_byte_fallback(self) -> bool {
        matches!(self, Self::Bytes(_))
    }
}

/// A wire position's character converted against its own file's text.
///
/// With text, the position's UTF-8 byte offset within its line becomes
/// a 1-indexed character count; without it — or on a line the text does
/// not contain — the byte offset shifts to 1-indexed as the fallback.
fn display_character(text: Option<&str>, position: lsp_types::Position) -> CharacterPosition {
    if let Some(text) = text
        && let Some(line_text) = text.lines().nth(position.line as usize)
    {
        let byte = usize::try_from(position.character).unwrap_or(line_text.len());
        let chars = line_text
            .char_indices()
            .take_while(|(offset, _)| *offset < byte)
            .count();
        let count = u32::try_from(chars).unwrap_or(u32::MAX).saturating_add(1);
        return CharacterPosition::Chars(count);
    }
    CharacterPosition::Bytes(position.character.saturating_add(1))
}

/// The distinct URIs among `locations` other than the queried file.
///
/// A definition batch commonly repeats a target (one trait method fanned
/// out per implementation); each distinct URI is read once regardless of
/// how many locations name it. Order-preserving.
fn distinct_foreign_uris(locations: &[lsp_types::Location], queried_uri: &str) -> Vec<String> {
    let mut seen: Vec<&str> = Vec::new();
    for location in locations {
        let uri = location.uri.as_str();
        if uri != queried_uri && !seen.contains(&uri) {
            seen.push(uri);
        }
    }
    seen.into_iter().map(String::from).collect()
}

/// Load the text of every distinct foreign file a definition batch names.
///
/// Each URI becomes a path, resolves against `cwd` under `policy`, and
/// reads once. A URI that is not a local `file://` path, escapes the
/// workspace under [`ResolvePolicy::Contained`], or fails to read is
/// simply absent from the map — its locations fall back to marked byte
/// offsets instead of failing the call.
async fn load_foreign_texts(
    locations: &[lsp_types::Location],
    queried_uri: &str,
    cwd: &Path,
    policy: ResolvePolicy,
) -> HashMap<String, String> {
    let mut texts = HashMap::new();
    for uri in distinct_foreign_uris(locations, queried_uri) {
        let Ok(url) = Url::parse(&uri) else {
            continue;
        };
        if url.scheme() != "file" {
            continue;
        }
        let Ok(path) = url.to_file_path() else {
            continue;
        };
        let Some(path) = path.to_str() else {
            continue;
        };
        let Ok(resolved) = resolve_path(path, cwd, policy) else {
            continue;
        };
        if let Ok(text) = tokio::fs::read_to_string(&resolved).await {
            texts.insert(uri, text);
        }
    }
    texts
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
/// `LocationLink` responses carry their target outside the `Location`
/// shape; they map to the target URI with the link's target selection
/// range, which preserves both where the target lives and the span of
/// its name.
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
                range: link.target_selection_range,
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
    fn schema_shape_is_snake_case() {
        let schema = LspTool.schema();
        let input = schema.input_schema;
        let properties = input
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        assert_eq!(properties.len(), 5, "exactly five properties");
        for key in [
            "operation",
            "file_path",
            "line",
            "character",
            "include_declaration",
        ] {
            assert!(properties.contains_key(key), "{key} present");
        }
        assert!(
            !properties.contains_key("filePath"),
            "the input field is snake_case, not camelCase"
        );
        let required = input.get("required").and_then(Value::as_array).unwrap();
        assert_eq!(
            required,
            json!(["operation", "file_path"]).as_array().unwrap()
        );
        let operations = input
            .pointer("/properties/operation/enum")
            .and_then(Value::as_array)
            .unwrap();
        for name in [
            "goToDefinition",
            "hover",
            "references",
            "implementations",
            "documentSymbol",
        ] {
            assert!(operations.contains(&json!(name)), "{name} in the enum");
        }
        assert!(
            !operations.contains(&json!("rename")),
            "the write-class exclusion stays excluded"
        );
        assert_eq!(
            input
                .pointer("/properties/line/minimum")
                .and_then(Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn document_symbol_needs_no_position_and_the_rest_do() {
        let document_symbol = parse_operation(&json!({
            "operation": "documentSymbol",
            "file_path": "src/lib.rs"
        }))
        .unwrap();
        assert_eq!(document_symbol, Operation::DocumentSymbol);
        for name in ["hover", "goToDefinition", "references", "implementations"] {
            let error = parse_operation(&json!({
                "operation": name,
                "file_path": "src/lib.rs"
            }))
            .unwrap_err();
            assert!(
                matches!(error, OperationError::Structural(ref e)
                    if matches!(e, ToolError::InvalidInput(field) if field.contains("line"))),
                "{name} without a position is invalid input: {error:?}"
            );
        }
    }

    #[test]
    fn include_declaration_defaults_to_true_and_rejects_non_booleans() {
        let references_input = |value: Value| {
            json!({
                "operation": "references",
                "file_path": "src/lib.rs",
                "line": 1,
                "character": 1,
                "include_declaration": value
            })
        };
        assert_eq!(
            parse_operation(&json!({
                "operation": "references",
                "file_path": "src/lib.rs",
                "line": 1,
                "character": 1
            }))
            .unwrap(),
            Operation::References {
                line: 1,
                character: 1,
                include_declaration: true
            }
        );
        assert_eq!(
            parse_operation(&references_input(json!(false))).unwrap(),
            Operation::References {
                line: 1,
                character: 1,
                include_declaration: false
            }
        );
        assert!(
            matches!(
                parse_operation(&references_input(json!("yes"))),
                Err(OperationError::Structural(ToolError::InvalidInput(_)))
            ),
            "a non-boolean is malformed input"
        );
    }

    #[tokio::test]
    async fn location_lists_cap_at_fifty_with_an_omitted_count() {
        use lsp_types::Range;
        use lsp_types::Uri;
        use std::str::FromStr;

        let uri = Url::from_file_path("/work/lib.rs").unwrap();
        let make = |line: u32| lsp_types::Location {
            uri: Uri::from_str(uri.as_str()).unwrap(),
            range: Range::new(Position::new(line, 0), Position::new(line, 1)),
        };
        let envelope = location_envelope(
            LocationQuery {
                operation: "references",
                file_path: "src/lib.rs",
                line: 1,
                character: 1,
                empty_message: "none",
                found: Some((0..55).map(make).collect()),
            },
            &uri,
            "fn add() {}\n",
            std::path::Path::new("/work"),
            ResolvePolicy::Unrestricted,
        )
        .await;
        assert_eq!(
            envelope
                .pointer("/result")
                .and_then(Value::as_array)
                .unwrap()
                .len(),
            50,
            "fifty-five locations cap at fifty"
        );
        assert_eq!(envelope.get("omitted"), Some(&json!(5)));
        assert_eq!(envelope.get("line"), Some(&json!(1)));

        let exact = location_envelope(
            LocationQuery {
                operation: "references",
                file_path: "src/lib.rs",
                line: 1,
                character: 1,
                empty_message: "none",
                found: Some((0..50).map(make).collect()),
            },
            &uri,
            "fn add() {}\n",
            std::path::Path::new("/work"),
            ResolvePolicy::Unrestricted,
        )
        .await;
        assert_eq!(
            exact
                .pointer("/result")
                .and_then(Value::as_array)
                .unwrap()
                .len(),
            50
        );
        assert_eq!(exact.get("omitted"), None, "an at-cap list is not cut");
    }

    #[test]
    fn document_symbols_cap_at_two_hundred_with_an_omitted_count() {
        let range = json!({
            "start": {"line": 0, "character": 0},
            "end": {"line": 0, "character": 3}
        });
        let outline = |count: usize| {
            let rows: Vec<Value> = (0..count)
                .map(|index| {
                    json!({
                        "name": format!("sym{index}"),
                        "kind": 12,
                        "location": {"uri": "file:///work/lib.rs", "range": range}
                    })
                })
                .collect();
            let response: lsp_types::DocumentSymbolResponse =
                serde_json::from_value(json!(rows)).unwrap();
            document_symbol_envelope("src/lib.rs", Some(response), "fn add() {}\n")
        };
        let capped = outline(205);
        assert_eq!(
            capped
                .pointer("/result")
                .and_then(Value::as_array)
                .unwrap()
                .len(),
            200,
            "two hundred five symbols cap at two hundred"
        );
        assert_eq!(capped.get("omitted"), Some(&json!(5)));
        let exact = outline(200);
        assert_eq!(
            exact
                .pointer("/result")
                .and_then(Value::as_array)
                .unwrap()
                .len(),
            200
        );
        assert_eq!(exact.get("omitted"), None, "an at-cap outline is not cut");
    }

    #[test]
    fn nested_outlines_count_descendants_toward_the_cap() {
        let range = json!({
            "start": {"line": 0, "character": 0},
            "end": {"line": 0, "character": 3}
        });
        let children: Vec<Value> = (0..204)
            .map(|index| {
                json!({
                    "name": format!("inner{index}"),
                    "kind": 8,
                    "range": range,
                    "selectionRange": range
                })
            })
            .collect();
        let reply = json!([{
            "name": "outer",
            "kind": 23,
            "range": range,
            "selectionRange": range,
            "children": children
        }]);
        let response: lsp_types::DocumentSymbolResponse = serde_json::from_value(reply).unwrap();
        let envelope = document_symbol_envelope("src/lib.rs", Some(response), "fn add() {}\n");
        assert_eq!(
            envelope
                .pointer("/result")
                .and_then(Value::as_array)
                .unwrap()
                .len(),
            200,
            "one parent plus two hundred four children cap at two hundred"
        );
        assert_eq!(
            envelope.get("omitted"),
            Some(&json!(5)),
            "the cut counts every flattened descendant, not top-level symbols: {envelope}"
        );
    }

    #[test]
    fn symbol_normalization_holds_the_cap_while_counting_further_symbols() {
        let range = json!({
            "start": {"line": 0, "character": 0},
            "end": {"line": 0, "character": 5}
        });
        let nested: lsp_types::DocumentSymbolResponse = serde_json::from_value(json!([{
            "name": "outer",
            "kind": 23,
            "range": range,
            "selectionRange": range,
            "children": [
                {"name": "first", "kind": 8, "range": range, "selectionRange": range},
                {"name": "second", "kind": 8, "range": range, "selectionRange": range}
            ]
        }]))
        .unwrap();
        let (rows, omitted) = normalize_symbols(nested, 2);
        assert_eq!(rows.len(), 2, "only the cap's worth of rows is held");
        assert_eq!(rows[0].name, "outer");
        assert_eq!(rows[1].name, "first");
        assert_eq!(
            rows[1].container.as_deref(),
            Some("outer"),
            "a retained descendant still names its parent"
        );
        assert_eq!(
            omitted, 1,
            "a symbol past the cap counts without becoming a row"
        );

        let flat: lsp_types::DocumentSymbolResponse = serde_json::from_value(json!([
            {"name": "a", "kind": 12, "location": {"uri": "file:///work/lib.rs", "range": range}},
            {"name": "b", "kind": 12, "location": {"uri": "file:///work/lib.rs", "range": range}},
            {"name": "c", "kind": 12, "location": {"uri": "file:///work/lib.rs", "range": range}}
        ]))
        .unwrap();
        let (rows, omitted) = normalize_symbols(flat, 2);
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"],
            "the leading rows survive the cut"
        );
        assert_eq!(omitted, 1);
    }

    #[tokio::test]
    async fn an_empty_location_list_and_an_absent_answer_render_differently() {
        let uri = Url::from_file_path("/work/lib.rs").unwrap();
        let empty = location_envelope(
            LocationQuery {
                operation: "references",
                file_path: "src/lib.rs",
                line: 1,
                character: 1,
                empty_message: "No references found at this location",
                found: Some(Vec::new()),
            },
            &uri,
            "fn add() {}\n",
            std::path::Path::new("/work"),
            ResolvePolicy::Unrestricted,
        )
        .await;
        assert_eq!(
            empty
                .pointer("/result")
                .and_then(Value::as_array)
                .unwrap()
                .len(),
            0,
            "an empty list renders as an empty result: {empty}"
        );
        assert_eq!(
            empty.get("message"),
            None,
            "an empty list carries no message: {empty}"
        );
        assert_eq!(empty.get("omitted"), None);

        let absent = location_envelope(
            LocationQuery {
                operation: "references",
                file_path: "src/lib.rs",
                line: 1,
                character: 1,
                empty_message: "No references found at this location",
                found: None,
            },
            &uri,
            "fn add() {}\n",
            std::path::Path::new("/work"),
            ResolvePolicy::Unrestricted,
        )
        .await;
        assert_eq!(
            absent.get("result"),
            Some(&json!(null)),
            "an absent answer reports a null result: {absent}"
        );
        assert_eq!(
            absent.get("message"),
            Some(&json!("No references found at this location")),
            "an absent answer takes the message path: {absent}"
        );
    }

    #[test]
    fn a_document_symbol_answer_of_none_reports_the_message() {
        let envelope = document_symbol_envelope("src/lib.rs", None, "fn add() {}\n");
        assert_eq!(
            envelope.get("result"),
            Some(&json!(null)),
            "an absent answer reports a null result: {envelope}"
        );
        assert_eq!(
            envelope.get("message"),
            Some(&json!("No symbols found in this document")),
            "the none path is the envelope's own standing message: {envelope}"
        );
    }

    #[tokio::test]
    async fn an_out_of_policy_target_names_itself_but_never_loads() {
        use lsp_types::Range;
        use lsp_types::Uri;
        use std::str::FromStr;

        let queried = Url::from_file_path("/work/lib.rs").unwrap();
        let foreign = lsp_types::Location {
            uri: Uri::from_str("file:///elsewhere/other.rs").unwrap(),
            range: Range::new(Position::new(0, 7), Position::new(0, 10)),
        };
        let envelope = location_envelope(
            LocationQuery {
                operation: "references",
                file_path: "src/lib.rs",
                line: 1,
                character: 1,
                empty_message: "none",
                found: Some(vec![foreign]),
            },
            &queried,
            "fn add() {}\n",
            std::path::Path::new("/work"),
            ResolvePolicy::Contained,
        )
        .await;
        let row = envelope.pointer("/result/0").expect("one row");
        assert_eq!(
            row.get("uri"),
            Some(&json!("file:///elsewhere/other.rs")),
            "the row still names where the target lives: {envelope}"
        );
        assert_eq!(
            row.get("character_units"),
            Some(&json!("utf-8-bytes")),
            "without the file's text the row falls back to marked bytes: {envelope}"
        );
    }

    #[test]
    fn hierarchical_symbols_flatten_with_container_names() {
        let range = json!({
            "start": {"line": 0, "character": 0},
            "end": {"line": 0, "character": 5}
        });
        let nested: lsp_types::DocumentSymbolResponse = serde_json::from_value(json!([{
            "name": "outer",
            "kind": 23,
            "range": range,
            "selectionRange": range,
            "children": [{
                "name": "middle",
                "kind": 6,
                "range": range,
                "selectionRange": range,
                "children": [{
                    "name": "leaf",
                    "kind": 8,
                    "range": range,
                    "selectionRange": range
                }]
            }]
        }]))
        .unwrap();
        let (rows, omitted) = normalize_symbols(nested, MAX_DOCUMENT_SYMBOLS);
        assert_eq!(rows.len(), 3, "every level becomes a row");
        assert_eq!(omitted, 0, "an under-cap outline omits nothing");
        assert_eq!(rows[0].name, "outer");
        assert_eq!(rows[0].container, None);
        assert_eq!(rows[1].container.as_deref(), Some("outer"));
        assert_eq!(rows[2].container.as_deref(), Some("middle"));

        let flat: lsp_types::DocumentSymbolResponse = serde_json::from_value(json!([{
            "name": "add",
            "kind": 12,
            "location": {
                "uri": "file:///work/lib.rs",
                "range": range
            },
            "containerName": "mod"
        }]))
        .unwrap();
        let (rows, omitted) = normalize_symbols(flat, MAX_DOCUMENT_SYMBOLS);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].container.as_deref(), Some("mod"));
        assert_eq!(omitted, 0);
    }

    #[test]
    fn symbol_kinds_map_to_lowercase_names() {
        use lsp_types::SymbolKind;
        assert_eq!(symbol_kind_name(SymbolKind::FUNCTION), "function");
        assert_eq!(symbol_kind_name(SymbolKind::STRUCT), "struct");
        assert_eq!(symbol_kind_name(SymbolKind::INTERFACE), "interface");
        assert_eq!(symbol_kind_name(SymbolKind::METHOD), "method");
        assert_eq!(symbol_kind_name(SymbolKind::MODULE), "module");
        let unnamed: SymbolKind = serde_json::from_value(json!(99)).unwrap();
        assert_eq!(
            symbol_kind_name(unnamed),
            "symbol",
            "a kind outside the named set still reports a name"
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

        let selection = Range::new(Position::new(2, 5), Position::new(2, 9));
        let link = GotoDefinitionResponse::Link(vec![LocationLink {
            origin_selection_range: None,
            target_uri: uri,
            target_range: Range::new(Position::new(1, 0), Position::new(3, 0)),
            target_selection_range: selection,
        }]);
        let locations = extract_goto_definition_locations(link);
        assert_eq!(locations.len(), 1);
        assert_eq!(
            locations[0].range, selection,
            "a link keeps its target selection range, not the enclosing range"
        );
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
    fn locations_report_character_counts_for_loaded_text() {
        use lsp_types::Range;
        use lsp_types::Uri;
        use std::str::FromStr;

        let text = "let s = \"héllo\";\n";
        let uri = Uri::from_str("file:///work/a.rs").unwrap();
        let location = lsp_types::Location {
            uri,
            range: Range::new(Position::new(0, 14), Position::new(0, 15)),
        };
        let converted = location_json(&location, Some(text));
        assert_eq!(
            converted.pointer("/range/start/character"),
            Some(&json!(14))
        );
        assert_eq!(converted.pointer("/range/end/character"), Some(&json!(15)));
        assert!(
            converted.get("character_units").is_none(),
            "a converted row carries no unit marker"
        );
    }

    #[test]
    fn locations_without_text_fall_back_to_marked_byte_offsets() {
        use lsp_types::Range;
        use lsp_types::Uri;
        use std::str::FromStr;

        let uri = Uri::from_str("file:///work/a.rs").unwrap();
        let location = lsp_types::Location {
            uri,
            range: Range::new(Position::new(0, 14), Position::new(0, 15)),
        };
        let fallback = location_json(&location, None);
        assert_eq!(
            fallback.pointer("/range/start/character"),
            Some(&json!(15)),
            "the wire byte offset shifts to 1-indexed"
        );
        assert_eq!(fallback.pointer("/range/end/character"), Some(&json!(16)));
        assert_eq!(
            fallback.get("character_units"),
            Some(&json!("utf-8-bytes")),
            "a fallback row names its unit"
        );
    }

    #[test]
    fn distinct_foreign_uris_skip_the_queried_file_and_collapse_repeats() {
        use lsp_types::Range;
        use lsp_types::Uri;
        use std::str::FromStr;

        let queried = "file:///work/lib.rs";
        let make = |uri: &str| lsp_types::Location {
            uri: Uri::from_str(uri).unwrap(),
            range: Range::default(),
        };
        let locations = vec![
            make(queried),
            make("file:///work/other.rs"),
            make("untitled:Untitled-1"),
            make("file:///work/other.rs"),
        ];
        let uris = distinct_foreign_uris(&locations, queried);
        assert_eq!(
            uris,
            vec![
                "file:///work/other.rs".to_string(),
                "untitled:Untitled-1".to_string()
            ],
            "the queried file drops out and repeats collapse"
        );
    }

    #[tokio::test]
    async fn foreign_texts_load_under_the_run_policy() {
        use lsp_types::Range;
        use lsp_types::Uri;
        use std::str::FromStr;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let inside_text = "/* café */ pub fn add() {}\n";
        std::fs::write(workspace.path().join("other.rs"), inside_text).unwrap();
        std::fs::write(outside.path().join("std.rs"), "pub fn map() {}\n").unwrap();
        let inside_uri = Url::from_file_path(workspace.path().join("other.rs"))
            .unwrap()
            .to_string();
        let outside_uri = Url::from_file_path(outside.path().join("std.rs"))
            .unwrap()
            .to_string();
        let make = |uri: &str| lsp_types::Location {
            uri: Uri::from_str(uri).unwrap(),
            range: Range::default(),
        };
        let locations = vec![
            make(&inside_uri),
            make(&inside_uri),
            make(&outside_uri),
            make("untitled:Untitled-1"),
        ];

        let contained = load_foreign_texts(
            &locations,
            "file:///work/lib.rs",
            workspace.path(),
            ResolvePolicy::Contained,
        )
        .await;
        assert_eq!(contained.len(), 1, "only the in-workspace file loads");
        assert_eq!(
            contained.get(&inside_uri).map(String::as_str),
            Some(inside_text)
        );

        let unrestricted = load_foreign_texts(
            &locations,
            "file:///work/lib.rs",
            workspace.path(),
            ResolvePolicy::Unrestricted,
        )
        .await;
        assert_eq!(
            unrestricted.len(),
            2,
            "the policy alone gates the out-of-workspace read"
        );
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
    async fn file_url_spellings_ask_for_a_filesystem_path() {
        let tmp = tempfile::tempdir().unwrap();
        let err = LspTool
            .call(lsp_input("file:///tmp/x.rs", 1, 1), &ctx_in(tmp.path()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("Pass a filesystem path")),
            "{err:?}"
        );
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
    async fn excluded_operations_stay_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["rename", "diagnostics", "completion"] {
            let mut input = lsp_input("src/lib.rs", 1, 1);
            input["operation"] = json!(name);
            let out = LspTool.call(input, &ctx_in(tmp.path())).await.unwrap();
            assert!(out.is_error, "soft error: {}", out.text_content());
            assert!(
                out.text_content()
                    .contains(&format!("Unknown operation: {name}")),
                "{}",
                out.text_content()
            );

            let bare = json!({
                "operation": name,
                "file_path": "src/lib.rs",
            });
            let out = LspTool.call(bare, &ctx_in(tmp.path())).await.unwrap();
            assert!(
                out.is_error,
                "soft error without a position: {}",
                out.text_content()
            );
            assert!(
                out.text_content()
                    .contains(&format!("Unknown operation: {name}")),
                "a schema-faithful call omits the position: {}",
                out.text_content()
            );
        }
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
    ///
    /// The appended `also_uses_add`, trait, and impl give the
    /// references, implementations, and documentSymbol tests their
    /// symbols without shifting the positions earlier tests pin.
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
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn uses_add() -> i32 {\n    add(1, 2)\n}\n\npub fn also_uses_add() -> i32 {\n    add(3, 4)\n}\n\npub trait Shape {\n    fn area(&self) -> i32;\n}\n\npub struct Unit;\n\nimpl Shape for Unit {\n    fn area(&self) -> i32 {\n        1\n    }\n}\n",
        )
        .unwrap();
        dir
    }

    /// A crate whose definition target lives in a second file.
    ///
    /// The definition sits behind multibyte content in `src/other.rs`,
    /// so a cross-file query observes whether foreign character
    /// positions convert against the target file's own text.
    fn fixture_crate_with_foreign_definition() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"lsp_fixture_foreign\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "mod other;\n\npub fn uses_other() -> i32 {\n    other::add(1, 2)\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/other.rs"),
            "/* café */ pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
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

    /// Whether a goToDefinition result carries the converted foreign
    /// location.
    ///
    /// The definition of `add` in `src/other.rs` starts at character 19
    /// — after `/* café */ pub fn `, where the é makes bytes and
    /// characters disagree; the byte fallback would report 20.
    fn foreign_target_reports_character_counts(text: &str) -> bool {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return false;
        };
        let Some(result) = value.get("result").and_then(Value::as_array) else {
            return false;
        };
        result.iter().any(|location| {
            location
                .get("uri")
                .and_then(Value::as_str)
                .is_some_and(|uri| uri.ends_with("other.rs"))
                && location.pointer("/range/start/character") == Some(&json!(19))
                && location.pointer("/range/start/line") == Some(&json!(1))
                && location.get("character_units").is_none()
        })
    }

    #[tokio::test]
    async fn goto_definition_converts_a_foreign_multibyte_target() {
        if !rust_analyzer_available() {
            return;
        }
        let _guard = SPAWN_GATE.lock().await;
        let dir = fixture_crate_with_foreign_definition();
        // `other::add(1, 2)` on line 4; the call's `add` starts at
        // character 12.
        let mut input = lsp_input("src/lib.rs", 4, 12);
        input["operation"] = json!("goToDefinition");
        let ctx = ctx_in(dir.path());
        eventually(
            || async {
                LspTool
                    .call(input.clone(), &ctx)
                    .await
                    .unwrap()
                    .text_content()
            },
            foreign_target_reports_character_counts,
        )
        .await;
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

    /// The 1-indexed start lines of a references envelope's rows.
    ///
    /// A live references answer's row order is not guaranteed, so the
    /// tests compare line sets rather than sequences; `None` means the
    /// envelope was empty or unparseable and the poll must continue.
    fn reference_lines(text: &str) -> Option<Vec<u64>> {
        let value: Value = serde_json::from_str(text).ok()?;
        let rows = value.pointer("/result")?.as_array()?;
        let mut lines = Vec::new();
        for row in rows {
            lines.push(row.pointer("/range/start/line")?.as_u64()?);
        }
        Some(lines)
    }

    #[tokio::test]
    async fn references_list_the_call_sites_and_honor_include_declaration() {
        if !rust_analyzer_available() {
            return;
        }
        let _guard = SPAWN_GATE.lock().await;
        let dir = fixture_crate();
        let ctx = ctx_in(dir.path());
        // `add(1, 2)` on line 6; the call starts at character 5.
        let mut input = lsp_input("src/lib.rs", 6, 5);
        input["operation"] = json!("references");
        input["include_declaration"] = json!(true);
        eventually(
            || async {
                let query = input.clone();
                LspTool.call(query, &ctx).await.unwrap().text_content()
            },
            |text| reference_lines(text).is_some_and(|lines| lines.len() == 3),
        )
        .await;
        let text = LspTool
            .call(input.clone(), &ctx)
            .await
            .unwrap()
            .text_content();
        let lines = reference_lines(&text).expect("parseable envelope");
        assert!(
            lines.contains(&1) && lines.contains(&6) && lines.contains(&10),
            "declaration and both call sites listed: {text}"
        );

        let mut without_declaration = input;
        without_declaration["include_declaration"] = json!(false);
        eventually(
            || async {
                let query = without_declaration.clone();
                LspTool.call(query, &ctx).await.unwrap().text_content()
            },
            |text| reference_lines(text).is_some_and(|lines| lines.len() == 2),
        )
        .await;
        let text = LspTool
            .call(without_declaration, &ctx)
            .await
            .unwrap()
            .text_content();
        let lines = reference_lines(&text).expect("parseable envelope");
        assert!(
            lines.contains(&6) && lines.contains(&10),
            "both call sites listed: {text}"
        );
        assert!(!lines.contains(&1), "the declaration drops out: {text}");
    }

    #[tokio::test]
    async fn implementations_list_the_trait_impl() {
        if !rust_analyzer_available() {
            return;
        }
        let _guard = SPAWN_GATE.lock().await;
        let dir = fixture_crate();
        let ctx = ctx_in(dir.path());
        // `pub trait Shape` on line 13; `Shape` starts at character 11.
        let mut input = lsp_input("src/lib.rs", 13, 11);
        input["operation"] = json!("implementations");
        eventually(
            || async {
                let query = input.clone();
                LspTool.call(query, &ctx).await.unwrap().text_content()
            },
            |text| {
                text.contains("\"operation\":\"implementations\"")
                    && (text.contains("\"result\":[") || text.contains("\"result\": ["))
            },
        )
        .await;
        let text = LspTool.call(input, &ctx).await.unwrap().text_content();
        assert!(text.contains("lib.rs"), "the impl lives in lib.rs: {text}");
    }

    #[tokio::test]
    async fn document_symbol_outlines_the_file_without_a_position() {
        if !rust_analyzer_available() {
            return;
        }
        let _guard = SPAWN_GATE.lock().await;
        let dir = fixture_crate();
        let input = json!({
            "operation": "documentSymbol",
            "file_path": "src/lib.rs",
        });
        let out = LspTool.call(input, &ctx_in(dir.path())).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(!text.contains("Unknown operation"), "{text}");
        assert!(text.contains("\"name\":\"add\""), "{text}");
        assert!(text.contains("\"name\":\"uses_add\""), "{text}");
        assert!(text.contains("\"name\":\"also_uses_add\""), "{text}");
        assert!(text.contains("\"kind\":\"function\""), "{text}");
        assert!(
            text.contains("\"kind\":\"interface\""),
            "the trait outlines as an interface: {text}"
        );
        assert!(
            text.contains("\"container\""),
            "the outline names containers: {text}"
        );
    }
}
