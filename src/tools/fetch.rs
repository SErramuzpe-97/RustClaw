//! `web_fetch`: retrieve a URL and hand the model readable text.
//!
//! HTML is stripped rather than parsed with a DOM library — a full parser is
//! several crates and a lot of allocation for what the model actually needs,
//! which is the prose.

use super::{Ctx, ToolOutput, ToolSpec, arg_str};
use serde_json::{Value, json};

const MAX_BYTES: usize = 256 * 1024;

pub const WEB_FETCH: ToolSpec = ToolSpec {
    name: "web_fetch",
    description: "Fetch a URL over HTTP(S) and return its text content, with HTML tags \
stripped. Use it to read documentation or an API response.",
    schema: || {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "Absolute http or https URL."}
            },
            "required": ["url"]
        })
    },
    run: |args, ctx| Box::pin(fetch(args, ctx)),
};

async fn fetch(args: Value, ctx: &Ctx) -> ToolOutput {
    let url = match arg_str(&args, "url") {
        Ok(u) => u,
        Err(e) => return e,
    };
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return ToolOutput::err("url must start with http:// or https://");
    }

    let resp = match ctx.http.get(url).header("accept", "text/*, application/json").send().await {
        Ok(r) => r,
        Err(e) => return ToolOutput::err(format!("request failed: {e}")),
    };
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => return ToolOutput::err(format!("could not read response body: {e}")),
    };

    let text = if content_type.contains("html") { strip_html(&body) } else { body };
    let text = super::exec::truncate_middle(&text, MAX_BYTES);

    if !status.is_success() {
        return ToolOutput::err(format!("HTTP {status}\n\n{text}"));
    }
    ToolOutput::ok(text)
}

/// Drop script/style bodies, then all tags, then collapse the whitespace that
/// HTML formatting leaves behind.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let bytes = html.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'<' {
            let rest = &html[i..];
            let lower_tag = rest
                .get(..16)
                .map(str::to_ascii_lowercase)
                .unwrap_or_else(|| rest.to_ascii_lowercase());
            // Skip the entire element for tags whose contents are not prose.
            for (open, close) in [("<script", "</script>"), ("<style", "</style>")] {
                if lower_tag.starts_with(open) {
                    match rest.to_ascii_lowercase().find(close) {
                        Some(end) => i += end + close.len(),
                        None => i = bytes.len(),
                    }
                    out.push(' ');
                    continue;
                }
            }
            if lower_tag.starts_with("<script") || lower_tag.starts_with("<style") {
                continue;
            }
            match rest.find('>') {
                Some(end) => {
                    i += end + 1;
                    out.push(' ');
                }
                None => break,
            }
        } else {
            let ch = html[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }

    decode_entities(&collapse_whitespace(&out))
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = false;
    for line in s.lines() {
        let t = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.is_empty() {
            if !blank && !out.is_empty() {
                out.push('\n');
                blank = true;
            }
        } else {
            out.push_str(&t);
            out.push('\n');
            blank = false;
        }
    }
    out.trim().to_string()
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_and_keeps_the_prose() {
        let html = "<html><body><h1>Title</h1><p>Hello <b>world</b>.</p></body></html>";
        assert_eq!(strip_html(html), "Title Hello world .");
    }

    #[test]
    fn drops_script_and_style_bodies_entirely() {
        let html = "<p>keep</p><script>var secret = 1;</script><style>.a{color:red}</style><p>also</p>";
        let out = strip_html(html);
        assert!(out.contains("keep"));
        assert!(out.contains("also"));
        assert!(!out.contains("secret"), "script body leaked: {out}");
        assert!(!out.contains("color"), "style body leaked: {out}");
    }

    #[test]
    fn decodes_the_common_entities() {
        assert_eq!(strip_html("<p>a &amp; b &lt;c&gt;</p>"), "a & b <c>");
    }

    #[test]
    fn handles_multibyte_text_without_panicking() {
        let out = strip_html("<p>árbol… 日本語</p>");
        assert!(out.contains("árbol"));
        assert!(out.contains("日本語"));
    }

    #[test]
    fn an_unterminated_tag_does_not_loop_forever() {
        assert_eq!(strip_html("text <unclosed"), "text");
    }

    #[tokio::test]
    async fn rejects_a_non_http_scheme() {
        let ctx = Ctx {
            cwd: std::env::temp_dir(),
            exec_timeout: std::time::Duration::from_secs(5),
            http: reqwest::Client::new(),
        };
        let out = fetch(json!({"url": "file:///etc/passwd"}), &ctx).await;
        assert!(out.is_error);
    }
}
