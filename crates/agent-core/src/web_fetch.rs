//! `web_fetch` tool: GET a single http(s) URL and return readable text.
//! HTML is reduced with a small hand-written extractor; the response body is
//! capped at 2 MB and the generic model-output projection in tools.rs handles
//! truncation plus saving the full result under .jucode/truncated-results.

use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;

const MAX_FETCH_BYTES: u64 = 2 * 1024 * 1024;
const ERROR_BODY_SNIPPET_BYTES: usize = 2048;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: u32 = 5;

pub fn definition() -> Value {
    json!({
        "type": "function",
        "name": "web_fetch",
        "description": "Fetch one http(s) URL with GET and return its readable text. Use it to read a URL the user referenced or documentation you already have the exact address for; it is not a search engine. HTML is converted to plain text (page title first, links as `text (url)`), text and JSON bodies pass through, and binary responses return metadata only. Large responses are truncated for you and the full text is saved to a file whose path is reported.",
        "parameters": {
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Absolute http:// or https:// URL to fetch." },
                "max_bytes": { "type": "number", "description": "Optional cap on bytes read from the response body. May only lower the 2 MB default." },
                "raw": { "type": "boolean", "description": "Return the raw body without HTML-to-text extraction. Defaults to false." }
            },
            "required": ["url"],
            "additionalProperties": false
        }
    })
}

pub fn run(args: &Value) -> Value {
    let Some(url) = args.get("url").and_then(Value::as_str) else {
        return json!({ "error": "missing url" });
    };
    if let Err(error) = validate_url(url) {
        return json!({ "url": url, "error": error });
    }
    let max_bytes = args
        .get("max_bytes")
        .and_then(Value::as_u64)
        .map(|value| value.clamp(1, MAX_FETCH_BYTES))
        .unwrap_or(MAX_FETCH_BYTES);
    let raw = args.get("raw").and_then(Value::as_bool).unwrap_or_default();
    fetch(url, max_bytes, raw)
}

fn fetch(url: &str, max_bytes: u64, raw: bool) -> Value {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .redirects(MAX_REDIRECTS)
        .build();
    let host = url_host(url);
    match agent.get(url).call() {
        Ok(response) => {
            let status = response.status();
            let final_url = response.get_url().to_string();
            let content_type = response
                .header("content-type")
                .unwrap_or_default()
                .to_string();
            let (body, network_truncated) = match read_capped(response.into_reader(), max_bytes) {
                Ok(read) => read,
                Err(error) => {
                    crate::log_warn!("web_fetch", "read failed", host = host, error = error);
                    return json!({ "url": url, "error": format!("failed to read response body: {error}") });
                }
            };
            crate::log_info!(
                "web_fetch",
                "fetched",
                host = host,
                status = status,
                bytes = body.len(),
            );
            process_response(
                &final_url,
                status,
                &content_type,
                &body,
                raw,
                network_truncated,
            )
        }
        Err(ureq::Error::Status(code, response)) => {
            let status_text = response.status_text().to_string();
            let snippet = response
                .into_string()
                .map(|body| body_snippet(&body))
                .unwrap_or_default();
            crate::log_warn!("web_fetch", "http error", host = host, status = code);
            json!({
                "url": url,
                "status": code,
                "error": format!("HTTP {code} {status_text}"),
                "body_snippet": snippet,
            })
        }
        Err(ureq::Error::Transport(transport)) => {
            let message = transport_error_message(&transport);
            crate::log_warn!("web_fetch", "transport error", host = host, error = message);
            json!({ "url": url, "error": message })
        }
    }
}

fn body_snippet(body: &str) -> String {
    let mut end = ERROR_BODY_SNIPPET_BYTES.min(body.len());
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    body[..end].to_string()
}

fn transport_error_message(transport: &ureq::Transport) -> String {
    let kind = match transport.kind() {
        ureq::ErrorKind::Dns => "DNS lookup failed",
        ureq::ErrorKind::ConnectionFailed => "connection failed",
        ureq::ErrorKind::Io => "network I/O error (possibly a timeout)",
        ureq::ErrorKind::InvalidUrl => "invalid URL",
        ureq::ErrorKind::TooManyRedirects => "too many redirects",
        _ => "request failed",
    };
    format!("{kind}: {transport}")
}

fn read_capped(mut reader: impl Read, max_bytes: u64) -> Result<(Vec<u8>, bool), String> {
    let mut body = Vec::new();
    let read = (&mut reader)
        .take(max_bytes)
        .read_to_end(&mut body)
        .map_err(|error| error.to_string())?;
    let mut probe = [0u8; 1];
    let truncated =
        read as u64 == max_bytes && reader.read(&mut probe).map_err(|error| error.to_string())? > 0;
    Ok((body, truncated))
}

/// Turn a fetched body into the tool result. Pure so tests can exercise the
/// content-type branches without touching the network.
fn process_response(
    url: &str,
    status: u16,
    content_type: &str,
    body: &[u8],
    raw: bool,
    network_truncated: bool,
) -> Value {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let mut result = json!({
        "url": url,
        "status": status,
        "content_type": mime,
        "bytes": body.len(),
    });
    if network_truncated {
        result["network_truncated"] = json!(true);
        result["note"] = json!("body exceeded the byte cap; only the first bytes were read");
    }
    let is_html = mime == "text/html" || mime == "application/xhtml+xml";
    let is_text = mime.starts_with("text/")
        || mime == "application/json"
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
        || mime == "application/xml"
        || mime == "application/javascript";
    if is_html && !raw {
        result["text"] = json!(extract_html_text(&String::from_utf8_lossy(body)));
    } else if is_html || is_text {
        result["text"] = json!(String::from_utf8_lossy(body).to_string());
    } else {
        result["note"] = json!("binary content omitted; only metadata is returned");
    }
    result
}

pub fn validate_url(url: &str) -> Result<(), String> {
    let rest = if let Some(rest) = url.strip_prefix("https://") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://") {
        rest
    } else {
        return Err("only http:// and https:// URLs are supported".to_string());
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err("URL has no host".to_string());
    }
    if authority.contains('@') {
        return Err("URLs with embedded credentials are not supported".to_string());
    }
    Ok(())
}

fn url_host(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_string()
}

// `title` is skipped because the extractor already emits it as the first line.
const SKIP_TAGS: [&str; 8] = [
    "script", "style", "head", "nav", "footer", "noscript", "template", "title",
];
const BLOCK_TAGS: [&str; 25] = [
    "p",
    "div",
    "br",
    "li",
    "ul",
    "ol",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "tr",
    "table",
    "section",
    "article",
    "header",
    "main",
    "aside",
    "blockquote",
    "pre",
    "hr",
    "form",
    "dt",
    "dd",
];

/// Reduce HTML to readable text: page title first, skip non-content elements
/// and comments, render links as `text (url)`, break on block elements,
/// decode common entities, and collapse whitespace runs.
pub fn extract_html_text(html: &str) -> String {
    let mut out = String::new();
    if let Some(title) = extract_title(html) {
        out.push_str(&title);
        out.push_str("\n\n");
    }
    let bytes = html.as_bytes();
    let mut i = 0;
    let mut link: Option<(String, String)> = None; // (href, buffered text)
    while i < bytes.len() {
        if bytes[i] != b'<' {
            let end = html[i..].find('<').map(|at| i + at).unwrap_or(html.len());
            let text = decode_entities(&html[i..end]);
            match &mut link {
                Some((_, buffered)) => buffered.push_str(&text),
                None => out.push_str(&text),
            }
            i = end;
            continue;
        }
        if html[i..].starts_with("<!--") {
            i = html[i..]
                .find("-->")
                .map(|at| i + at + 3)
                .unwrap_or(html.len());
            continue;
        }
        let tag_end = html[i..].find('>').map(|at| i + at).unwrap_or(html.len());
        let tag_body = &html[i + 1..tag_end.min(html.len())];
        i = (tag_end + 1).min(html.len());
        let closing = tag_body.starts_with('/');
        let name = tag_name(tag_body);
        if name.is_empty() {
            continue;
        }
        if !closing && SKIP_TAGS.contains(&name.as_str()) {
            i = skip_element(html, i, &name);
            continue;
        }
        if name == "a" {
            if closing {
                if let Some((href, text)) = link.take() {
                    out.push_str(&render_link(&text, &href));
                }
            } else {
                // Flush any unterminated previous link as plain text.
                if let Some((_, text)) = link.take() {
                    out.push_str(&text);
                }
                link = Some((
                    attr_value(tag_body, "href").unwrap_or_default(),
                    String::new(),
                ));
            }
            continue;
        }
        if BLOCK_TAGS.contains(&name.as_str()) {
            let target = match &mut link {
                Some((_, buffered)) => buffered,
                None => &mut out,
            };
            // Closing list items add no break so consecutive items stay adjacent.
            if name == "li" {
                if !closing {
                    target.push_str("\n- ");
                }
            } else {
                target.push('\n');
            }
        }
    }
    if let Some((_, text)) = link {
        out.push_str(&text);
    }
    collapse_whitespace(&out)
}

fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open_end = html[start..].find('>').map(|at| start + at + 1)?;
    let close = lower[open_end..].find("</title").map(|at| open_end + at)?;
    let title = collapse_whitespace(&decode_entities(&html[open_end..close]));
    (!title.is_empty()).then_some(title)
}

fn tag_name(tag_body: &str) -> String {
    tag_body
        .trim_start_matches('/')
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn skip_element(html: &str, from: usize, name: &str) -> usize {
    let lower = html[from..].to_ascii_lowercase();
    let closer = format!("</{name}");
    match lower.find(&closer) {
        Some(at) => {
            let after = from + at + closer.len();
            html[after..]
                .find('>')
                .map(|gt| after + gt + 1)
                .unwrap_or(html.len())
        }
        None => html.len(),
    }
}

fn attr_value(tag_body: &str, attr: &str) -> Option<String> {
    let lower = tag_body.to_ascii_lowercase();
    let needle = format!("{attr}=");
    let mut search = 0;
    loop {
        let at = lower[search..].find(&needle)? + search;
        let before = lower[..at].chars().next_back();
        if before.is_some_and(|ch| !ch.is_whitespace()) {
            search = at + needle.len();
            continue;
        }
        let rest = &tag_body[at + needle.len()..];
        let value = match rest.chars().next() {
            Some(quote @ ('"' | '\'')) => rest[1..].split(quote).next().unwrap_or_default(),
            _ => rest.split_whitespace().next().unwrap_or_default(),
        };
        return Some(decode_entities(value));
    }
}

fn render_link(text: &str, href: &str) -> String {
    let text = collapse_whitespace(text);
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') || href == text {
        return text;
    }
    if text.is_empty() {
        return href.to_string();
    }
    format!("{text} ({href})")
}

pub fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest[..rest.len().min(12)].find(';');
        let Some(end) = end else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => decode_numeric_entity(entity),
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_numeric_entity(entity: &str) -> Option<char> {
    let digits = entity.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<u32>().ok()?,
    };
    char::from_u32(code)
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank_run > 0 {
                out.push('\n');
            }
        }
        blank_run = 0;
        out.push_str(&line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_url_accepts_only_plain_http_and_https() {
        assert!(validate_url("https://example.com/docs?q=1").is_ok());
        assert!(validate_url("http://example.com").is_ok());
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("https://").is_err());
        assert!(validate_url("example.com").is_err());
    }

    #[test]
    fn validate_url_rejects_embedded_credentials() {
        assert!(validate_url("https://user:pass@example.com/").is_err());
        assert!(validate_url("http://admin@example.com").is_err());
        // '@' after the authority is fine.
        assert!(validate_url("https://example.com/path?email=a@b.c").is_ok());
    }

    #[test]
    fn run_reports_missing_or_invalid_url() {
        assert!(run(&json!({})).get("error").is_some());
        let result = run(&json!({ "url": "ftp://example.com" }));
        assert!(result["error"]
            .as_str()
            .unwrap()
            .contains("http:// and https://"));
    }

    #[test]
    fn extractor_strips_non_content_elements_and_comments() {
        let html = "<html><head><title>Doc</title><style>p{color:red}</style></head>\
            <body><nav>menu</nav><script>alert(1)</script><!-- hidden -->\
            <p>Hello world</p><footer>legal</footer></body></html>";
        let text = extract_html_text(html);
        assert_eq!(text, "Doc\n\nHello world");
    }

    #[test]
    fn extractor_renders_links_with_href() {
        let html = r#"<p>See <a href="https://example.com/a">the docs</a> now.</p>"#;
        let text = extract_html_text(html);
        assert_eq!(text, "See the docs (https://example.com/a) now.");
    }

    #[test]
    fn extractor_skips_fragment_and_self_referencing_links() {
        let html = r##"<a href="#top">Top</a> <a href="https://x.io">https://x.io</a>"##;
        let text = extract_html_text(html);
        assert_eq!(text, "Top https://x.io");
    }

    #[test]
    fn extractor_decodes_common_entities() {
        let html = "<p>a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#39; &#x41;&nbsp;f</p>";
        assert_eq!(extract_html_text(html), "a & b <c> \"d\" 'e' A f");
        assert_eq!(decode_entities("1 &unknown; 2 & 3"), "1 &unknown; 2 & 3");
    }

    #[test]
    fn extractor_breaks_on_block_elements_and_collapses_blanks() {
        let html = "<h1>Title</h1><div><div><p>one</p></div></div><ul><li>a</li><li>b</li></ul>";
        let text = extract_html_text(html);
        assert_eq!(text, "Title\n\none\n\n- a\n- b");
    }

    #[test]
    fn extractor_puts_page_title_first() {
        let html = "<title>My &amp; Page</title><p>body text</p>";
        assert_eq!(extract_html_text(html), "My & Page\n\nbody text");
    }

    #[test]
    fn process_response_extracts_html_text() {
        let body = b"<html><title>T</title><body><p>hi</p></body></html>";
        let result = process_response(
            "https://example.com",
            200,
            "text/html; charset=utf-8",
            body,
            false,
            false,
        );
        assert_eq!(result["status"], 200);
        assert_eq!(result["content_type"], "text/html");
        assert_eq!(result["text"], "T\n\nhi");
        assert_eq!(result["bytes"], body.len());
    }

    #[test]
    fn process_response_passes_text_and_json_through() {
        let result = process_response("u", 200, "application/json", b"{\"a\":1}", false, false);
        assert_eq!(result["text"], "{\"a\":1}");
        let result = process_response("u", 200, "text/plain", b"plain", false, false);
        assert_eq!(result["text"], "plain");
    }

    #[test]
    fn process_response_raw_skips_html_extraction() {
        let result = process_response("u", 200, "text/html", b"<p>hi</p>", true, false);
        assert_eq!(result["text"], "<p>hi</p>");
    }

    #[test]
    fn process_response_returns_metadata_only_for_binary() {
        let result = process_response("u", 200, "image/png", &[0x89, 0x50], false, false);
        assert!(result.get("text").is_none());
        assert_eq!(result["bytes"], 2);
        assert!(result["note"].as_str().unwrap().contains("binary"));
    }

    #[test]
    fn process_response_marks_network_truncation() {
        let result = process_response("u", 200, "text/plain", b"abc", false, true);
        assert_eq!(result["network_truncated"], true);
    }

    #[test]
    fn read_capped_stops_at_limit_and_flags_truncation() {
        let (body, truncated) = read_capped(&b"0123456789"[..], 4).unwrap();
        assert_eq!(body, b"0123");
        assert!(truncated);
        let (body, truncated) = read_capped(&b"0123"[..], 4).unwrap();
        assert_eq!(body, b"0123");
        assert!(!truncated);
        let (body, truncated) = read_capped(&b"01"[..], 4).unwrap();
        assert_eq!(body, b"01");
        assert!(!truncated);
    }
}
