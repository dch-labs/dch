//! The `WebFetch` tool — fetch a URL over HTTP(S) and return it as markdown or text.

use std::fmt::Write;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures::StreamExt;
use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use loopctl::tool::ToolSchema;
use serde_json::Value;
use serde_json::json;
use url::Url;

use crate::input::get_u64;

/// Default request timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 20;

/// Hard ceiling on a request timeout.
///
/// Larger input values are clamped rather than rejected, so an
/// over-ambitious request still runs with the maximum allowed wait —
/// the same contract the Bash tool offers.
const MAX_TIMEOUT_SECS: u64 = 600;

/// Maximum number of HTTP redirects to follow.
const MAX_REDIRECTS: usize = 5;

/// Hard cap on a response body before truncation, in bytes.
///
/// The streamed read never retains more than this plus one chunk, so the
/// cap bounds memory as well as output size.
const MAX_BODY_BYTES: usize = 1_000_000;

/// The output format the model requested for HTML bodies.
///
/// Parsed strictly: an input value outside the two variants is rejected as
/// invalid input rather than silently coerced.
#[derive(Clone, Copy)]
enum OutputFormat {
    /// Convert the HTML to `CommonMark` markdown.
    ///
    /// Headings, emphasis, links, and code blocks keep their markdown
    /// shape, so the model receives the same structure a rendered page
    /// would show. This is the behavior when the request omits `format`.
    Markdown,

    /// Convert the HTML to markdown, then strip the markdown sigils.
    ///
    /// Intended for requests that want prose rather than markup: the
    /// stripping pass removes heading hashes, bold markers, and link
    /// targets while keeping the visible text intact.
    Text,
}

/// A response body read under the byte cap.
///
/// Produced by the streamed body read: the retained bytes and the record
/// of whether anything was left behind travel together, so the renderer
/// can append the truncation marker without re-measuring the transfer.
struct CappedBody {
    /// The retained body bytes.
    ///
    /// Never longer than [`MAX_BODY_BYTES`] — the streamed read stops
    /// appending at the cap and abandons the transfer, so this buffer's
    /// length is also the fetch's memory bound.
    bytes: Vec<u8>,

    /// Whether the transfer was abandoned before its end.
    ///
    /// True only when bytes were actually left behind: a body that ends
    /// exactly at the cap is complete, not truncated, and must not carry
    /// the marker.
    truncated: bool,
}

/// Fetch content from a URL and return it in a text format for processing.
///
/// The tool performs HTTP(S) GETs with rustls, follows up to five
/// redirects, and bounds every response body at one million bytes by
/// abandoning the transfer once the cap is reached. It never writes to
/// the local filesystem, but it does hit third-party servers, so calls
/// run serialized against each other.
pub struct WebFetchTool;

impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "WebFetch"
    }

    fn description(&self) -> &'static str {
        "Fetch content from a URL and return it in a text format suitable for \
         processing. Use this to retrieve web pages, documentation, or other \
         online resources."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Request timeout in seconds (default: 20, max: 600)",
                        "default": DEFAULT_TIMEOUT_SECS,
                        "minimum": 1,
                        "maximum": MAX_TIMEOUT_SECS
                    },
                    "format": {
                        "type": "string",
                        "enum": ["markdown", "text"],
                        "description": "Output format (default: markdown)",
                        "default": "markdown"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    fn call(
        &self,
        input: Value,
        _ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        Box::pin(self.call_inner(input))
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }
}

impl WebFetchTool {
    /// Body of [`Tool::call`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] for a missing or malformed `url`, an
    /// out-of-range `timeout`, a `format` outside the schema enum, a
    /// non-http(s) scheme, a client build failure, or a transport failure
    /// (timeout, redirect excess, DNS/TLS/connection errors).
    async fn call_inner(&self, input: Value) -> Result<ToolOutput, ToolError> {
        let raw_url = input
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("Missing url".to_string()))?;
        let timeout_secs = parse_timeout(&input)?;
        let format = parse_format(&input)?;
        let url = check_url(raw_url)?;

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .timeout(Duration::from_secs(timeout_secs))
            .user_agent("dch/1.0")
            .build()
            .map_err(|e| ToolError::Execution(format!("Failed to create HTTP client: {e}")))?;

        let response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| map_transport_error(&e, timeout_secs))?;

        if !response.status().is_success() {
            return Ok(ToolOutput::error_text(format!(
                "HTTP {} from {url}",
                response.status()
            )));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("text/plain")
            .to_string();

        let capped = read_body_capped(response).await?;
        let body = capped_body_to_text(&capped);
        Ok(build_output(&body, &content_type, &url, format))
    }
}

/// Parse the `timeout` field with the shared loud-failure contract.
///
/// Absent means the default; zero is rejected; values above the ceiling are
/// clamped to it.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when the field is present but not a
/// non-negative integer, or when it is zero.
fn parse_timeout(input: &Value) -> Result<u64, ToolError> {
    let timeout = get_u64(input, "timeout")?
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .min(MAX_TIMEOUT_SECS);
    if timeout == 0 {
        return Err(ToolError::InvalidInput(
            "'timeout' must be at least 1, got 0".to_string(),
        ));
    }
    Ok(timeout)
}

/// Parse the `format` field, rejecting values outside the schema enum.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when the field is present but is not
/// exactly `markdown` or `text`.
fn parse_format(input: &Value) -> Result<OutputFormat, ToolError> {
    match input.get("format").and_then(Value::as_str) {
        None | Some("markdown") => Ok(OutputFormat::Markdown),
        Some("text") => Ok(OutputFormat::Text),
        Some(other) => Err(ToolError::InvalidInput(format!(
            "'format' must be one of: markdown, text (got '{other}')"
        ))),
    }
}

/// Parse and gate the target URL.
///
/// `Url::parse` alone accepts schemes this tool must never touch (`file:`,
/// `ftp:`, …), so the scheme allowlist is enforced explicitly.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when the URL cannot be parsed or its
/// scheme is not `http`/`https`.
fn check_url(raw: &str) -> Result<Url, ToolError> {
    let parsed =
        Url::parse(raw).map_err(|e| ToolError::InvalidInput(format!("Invalid URL: {e}")))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(ToolError::InvalidInput(
            "Only http and https URLs are supported".to_string(),
        ));
    }
    Ok(parsed)
}

/// Classify a transport failure into its hard-error message.
///
/// Timeouts, redirect excess, and lower-level failures each get their own
/// wording so the model can tell an over-eager redirect chain from a dead
/// host from a too-short timeout.
fn map_transport_error(e: &reqwest::Error, timeout_secs: u64) -> ToolError {
    if e.is_redirect() {
        ToolError::Execution(format!(
            "Too many redirects (more than {MAX_REDIRECTS}) or redirect loop"
        ))
    } else if e.is_timeout() {
        ToolError::Execution(format!("Request timed out after {timeout_secs}s"))
    } else {
        ToolError::Execution(format!("Request failed: {e}"))
    }
}

/// Read a response body while retaining at most [`MAX_BODY_BYTES`] bytes.
///
/// Chunks are appended only while the buffer is below the cap; once it is
/// reached the stream is dropped, abandoning the transfer. Memory stays
/// bounded by the cap plus one chunk no matter how large the body claims
/// or actually is. A body that ends exactly at the cap is not truncated;
/// the marker is reserved for bytes that were actually left behind.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] when a chunk fails to arrive.
async fn read_body_capped(response: reqwest::Response) -> Result<CappedBody, ToolError> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    let mut truncated = false;
    loop {
        if bytes.len() >= MAX_BODY_BYTES {
            if stream.next().await.is_some() {
                truncated = true;
            }
            break;
        }
        let Some(chunk) = stream.next().await else {
            break;
        };
        let chunk =
            chunk.map_err(|e| ToolError::Execution(format!("Failed to read response: {e}")))?;
        let room = MAX_BODY_BYTES.saturating_sub(bytes.len());
        if chunk.len() > room {
            bytes.extend_from_slice(chunk.get(..room).unwrap_or(&chunk));
            truncated = true;
        } else {
            bytes.extend_from_slice(&chunk);
        }
    }
    Ok(CappedBody { bytes, truncated })
}

/// Render a capped body as text, marking truncation.
///
/// The cut lands at the byte cap — or the body's end, whichever comes
/// first — and backs off to a UTF-8 char boundary before the lossy
/// conversion, so a multi-byte sequence split by the cap does not surface
/// as a replacement character; the conversion is still lossy because a
/// body need not be UTF-8 at all.
fn capped_body_to_text(body: &CappedBody) -> String {
    let mut cut = body.bytes.len().min(MAX_BODY_BYTES);
    while cut > 0
        && body
            .bytes
            .get(cut)
            .is_some_and(|byte| is_continuation(*byte))
    {
        cut = cut.saturating_sub(1);
    }
    let kept = body.bytes.get(..cut).unwrap_or(&body.bytes);
    let mut text = String::from_utf8_lossy(kept).into_owned();
    if body.truncated {
        write!(text, "\n[response truncated at {MAX_BODY_BYTES} bytes]").ok();
    }
    text
}

/// Whether `byte` is a UTF-8 continuation byte.
///
/// A byte index whose byte is not a continuation is a char boundary, so
/// backing the cut off past continuations lands it on one.
fn is_continuation(byte: u8) -> bool {
    (0x80..=0xBF).contains(&byte)
}

/// Build the tool output for a body under its declared content type.
///
/// HTML is converted per the requested format; JSON is pretty-printed in a
/// fenced block (raw on parse failure); anything else is returned as-is
/// with a note naming its content type.
fn build_output(body: &str, content_type: &str, url: &Url, format: OutputFormat) -> ToolOutput {
    if content_type.contains("text/html") {
        match convert_html(body, format) {
            Ok(text) => ToolOutput::text(format!("URL: {url}\n\n{text}")),
            Err(e) => ToolOutput::error_text(e.to_string()),
        }
    } else if content_type.contains("application/json") {
        match serde_json::from_str::<Value>(body) {
            Ok(json) => {
                let pretty =
                    serde_json::to_string_pretty(&json).unwrap_or_else(|_| body.to_string());
                ToolOutput::text(format!(
                    "URL: {url}\nContent-Type: {content_type}\n\n```json\n{pretty}\n```"
                ))
            }
            Err(_) => ToolOutput::text(format!(
                "URL: {url}\nContent-Type: {content_type}\n\n{body}"
            )),
        }
    } else {
        ToolOutput::text(format!(
            "URL: {url}\nContent-Type: {content_type}\n(non-HTML content returned as-is)\n\n{body}"
        ))
    }
}

/// Convert an HTML body per the requested output format.
///
/// Both formats share one DOM-based conversion, with script and style
/// elements skipped wholesale; text mode additionally strips the markdown
/// sigils the conversion introduces.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] when the converter fails on the input.
fn convert_html(html: &str, format: OutputFormat) -> Result<String, ToolError> {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style"])
        .build();
    let markdown = converter
        .convert(html)
        .map_err(|e| ToolError::Execution(format!("HTML conversion failed: {e}")))?;
    match format {
        OutputFormat::Markdown => Ok(markdown),
        OutputFormat::Text => Ok(strip_markdown_sigils(&markdown)),
    }
}

/// Remove the markdown sigils a conversion introduces.
///
/// Headings lose their leading hashes, bold markers are dropped, and links
/// collapse to their label — enough shape removal that a `text` request
/// reads as prose rather than as markdown source.
fn strip_markdown_sigils(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    for line in markdown.lines() {
        let stripped = line.trim_start_matches('#');
        let stripped = stripped.strip_prefix(' ').unwrap_or(stripped);
        let stripped = stripped.replace("**", "");
        let stripped = unwrap_links(&stripped);
        out.push_str(&stripped);
        out.push('\n');
    }
    out.trim().to_string()
}

/// Collapse `[label](target)` spans to their label.
fn unwrap_links(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some((before, after_open)) = rest.split_once('[') {
        let Some((label, after_label)) = after_open.split_once("](") else {
            out.push_str(before);
            out.push('[');
            out.push_str(after_open);
            return out;
        };
        out.push_str(before);
        out.push_str(label);
        if let Some((_, after_close)) = after_label.split_once(')') {
            rest = after_close;
        } else {
            out.push_str(after_label);
            return out;
        }
    }
    out.push_str(rest);
    out
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
    use serde_json::json;

    #[test]
    fn test_convert_html_markdown() {
        let html = "<h1>T</h1><p>hi <b>x</b></p>";
        let out = convert_html(html, OutputFormat::Markdown).unwrap();
        assert!(out.contains("# T"), "heading survived: {out}");
        assert!(out.contains("**x**"), "bold survived: {out}");
    }

    #[test]
    fn test_convert_html_text() {
        let html = "<h1>T</h1><p>hi <b>x</b></p>";
        let out = convert_html(html, OutputFormat::Text).unwrap();
        assert!(out.contains('T'), "text survived: {out}");
        assert!(out.contains('x'), "text survived: {out}");
        assert!(!out.contains('#'), "no heading sigil: {out}");
        assert!(!out.contains("**"), "no bold sigil: {out}");
    }

    #[test]
    fn test_convert_html_entities() {
        let html = "<p>AT&amp;T &lt;10&gt;</p>";
        let out = convert_html(html, OutputFormat::Text).unwrap();
        assert!(out.contains("AT&T <10>"), "entities decoded: {out}");
    }

    #[test]
    fn test_convert_html_strips_script_style() {
        let html = "<script>alert(1)</script><style>s{}</style><p>ok</p>";
        let out = convert_html(html, OutputFormat::Text).unwrap();
        assert!(out.contains("ok"), "body survived: {out}");
        assert!(!out.contains("alert"), "script dropped: {out}");
        assert!(!out.contains("s{}"), "style dropped: {out}");
    }

    #[test]
    fn test_content_type_json_pretty() {
        let url = Url::parse("https://example.com/api").unwrap();
        let out = build_output(
            r#"{"b":1,"a":2}"#,
            "application/json",
            &url,
            OutputFormat::Markdown,
        );
        assert!(!out.is_error);
        let text = out.text_content();
        assert!(text.contains("```json"), "fenced: {text}");
        assert!(text.contains("\"a\": 2"), "pretty-printed: {text}");
    }

    #[test]
    fn test_content_type_json_invalid_falls_back() {
        let url = Url::parse("https://example.com/api").unwrap();
        let out = build_output(
            "not json at all",
            "application/json",
            &url,
            OutputFormat::Markdown,
        );
        assert!(!out.is_error);
        assert!(out.text_content().contains("not json at all"));
    }

    #[test]
    fn test_content_type_other_returned_as_is() {
        let url = Url::parse("https://example.com/f.txt").unwrap();
        let out = build_output("plain body", "text/plain", &url, OutputFormat::Markdown);
        assert!(!out.is_error);
        let text = out.text_content();
        assert!(text.contains("plain body"), "raw body kept: {text}");
        assert!(
            text.contains("non-HTML content returned as-is"),
            "note present: {text}"
        );
        assert!(text.contains("Content-Type: text/plain"), "typed: {text}");
    }

    #[test]
    fn test_content_type_charset_suffix() {
        let url = Url::parse("https://example.com").unwrap();
        let out = build_output(
            "<h1>T</h1><p>ok</p>",
            "text/html; charset=utf-8",
            &url,
            OutputFormat::Markdown,
        );
        assert!(!out.is_error);
        assert!(out.text_content().contains("# T"), "HTML branch taken");
    }

    #[test]
    fn test_capped_body_under_cap_passes_through() {
        let body = CappedBody {
            bytes: b"short body".to_vec(),
            truncated: false,
        };
        let text = capped_body_to_text(&body);
        assert_eq!(text, "short body");
    }

    #[test]
    fn test_capped_body_over_cap_truncates_at_boundary() {
        let core = "€".repeat(400_000);
        let mut bytes = core.into_bytes();
        bytes.push(b'x');
        while bytes.len() < MAX_BODY_BYTES + 50_000 {
            bytes.extend_from_slice(b"y");
        }
        let body = CappedBody {
            bytes,
            truncated: true,
        };
        let text = capped_body_to_text(&body);
        assert!(
            text.ends_with(&format!("\n[response truncated at {MAX_BODY_BYTES} bytes]")),
            "marker appended: {}",
            &text[text.len().saturating_sub(80)..]
        );
        assert!(text.len() < MAX_BODY_BYTES + 100, "output bounded");
        assert!(!text.contains('\u{FFFD}'), "no replacement char");
    }

    #[tokio::test]
    async fn test_url_scheme_rejected() {
        let tool = WebFetchTool;
        for bad in ["file:///etc/passwd", "ftp://x", "not a url"] {
            let err = tool
                .call(json!({ "url": bad }), &ToolContext::default())
                .await
                .unwrap_err();
            assert!(
                matches!(err, ToolError::InvalidInput(_)),
                "{bad} rejected: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_missing_url_errors() {
        let tool = WebFetchTool;
        let err = tool
            .call(json!({}), &ToolContext::default())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("url")),
            "missing url named: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_format_value_outside_enum_rejected() {
        let tool = WebFetchTool;
        let err = tool
            .call(
                json!({ "url": "https://example.com", "format": "html" }),
                &ToolContext::default(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("markdown") && s.contains("text")),
            "valid values named: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_timeout_zero_rejected() {
        let tool = WebFetchTool;
        let err = tool
            .call(
                json!({ "url": "https://example.com", "timeout": 0 }),
                &ToolContext::default(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("timeout")),
            "zero timeout rejected before IO: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_streamed_body_over_cap_truncates_with_marker() {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Drain the request before closing: a socket dropped with unread
            // receive data resets the connection and surfaces as a mid-body
            // ConnectionReset on the client instead of a clean end.
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.ends_with(b"\r\n\r\n") && request.len() < 16_384 {
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            let body = vec![b'a'; MAX_BODY_BYTES + 50_000];
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.ok();
            // The client abandons the transfer once it has its capped bytes,
            // so the tail write may fail with a broken pipe — that is the
            // behavior under test, not an error.
            socket.write_all(&body).await.ok();
            socket.shutdown().await.ok();
        });

        let tool = WebFetchTool;
        let out = tool
            .call(
                json!({ "url": format!("http://{addr}/big") }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(
            text.contains(&format!("[response truncated at {MAX_BODY_BYTES} bytes]")),
            "marker present: {}",
            &text[text.len().saturating_sub(80)..]
        );
        assert!(text.len() < MAX_BODY_BYTES + 200, "bounded: {}", text.len());
        server.await.unwrap();
    }

    #[test]
    fn test_webfetchtool_registered_in_builtin_registry() {
        let reg = crate::registry::builtin_registry();
        let tool = reg.get("WebFetch").expect("WebFetch registered");
        assert!(tool.is_read_only());
        assert!(!tool.is_concurrency_safe());
    }

    #[test]
    fn test_webfetchtool_schema_matches_spec() {
        let schema = WebFetchTool.schema();
        let input = schema.input_schema;
        let required = input.get("required").and_then(|v| v.as_array()).unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "url");
        let format = input.pointer("/properties/format").unwrap();
        assert_eq!(
            format.get("enum").and_then(|v| v.as_array()).unwrap(),
            &["markdown".to_string(), "text".to_string()]
        );
        let timeout = input.pointer("/properties/timeout").unwrap();
        assert_eq!(timeout.get("default").and_then(|v| v.as_u64()), Some(20));
        assert_eq!(timeout.get("minimum").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(timeout.get("maximum").and_then(|v| v.as_u64()), Some(600));
    }

    #[tokio::test]
    #[ignore = "hits the real network; run manually with --ignored"]
    async fn test_webfetch_real_url_markdown() {
        let tool = WebFetchTool;
        let out = tool
            .call(
                json!({ "url": "https://example.com", "format": "markdown" }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(text.starts_with("URL: https://example.com"), "{text}");
        assert!(
            text.contains("Example") || text.contains("example.com"),
            "{text}"
        );
    }

    #[tokio::test]
    #[ignore = "hits the real network; run manually with --ignored"]
    async fn test_webfetch_real_404_is_soft_error() {
        let tool = WebFetchTool;
        let out = tool
            .call(
                json!({ "url": "https://example.com/definitely-missing-404" }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "404 must be a soft error, not Ok-clean");
        assert!(out.text_content().contains("404"), "{}", out.text_content());
    }

    #[tokio::test]
    #[ignore = "hits the real network; run manually with --ignored"]
    async fn test_webfetch_real_timeout() {
        let tool = WebFetchTool;
        let err = tool
            .call(
                json!({ "url": "https://httpbin.org/delay/10", "timeout": 1 }),
                &ToolContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)), "{err:?}");
        assert!(err.to_string().contains("timed out"), "{err}");
    }
}
