// ===========================================================================
// Telegram formatting helpers — markdown-to-HTML conversion and message splitting.
// ===========================================================================

use super::MAX_MESSAGE_LEN;

pub fn split_for_telegram(text: &str) -> Vec<String> {
    split_for_telegram_at(text, MAX_MESSAGE_LEN)
}

pub fn split_for_telegram_at(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let mut parts = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            parts.push(remaining.to_string());
            break;
        }

        // Find a split point at max_len, respecting UTF-8 boundaries.
        let mut end = max_len;
        while !remaining.is_char_boundary(end) && end > 0 {
            end -= 1;
        }

        // Try to split at the last newline within the chunk for cleaner breaks.
        if let Some(nl) = remaining[..end].rfind('\n') {
            parts.push(remaining[..nl].to_string());
            remaining = &remaining[nl + 1..];
        } else {
            parts.push(remaining[..end].to_string());
            remaining = &remaining[end..];
        }
    }

    parts
}

/// Convert standard markdown to Telegram-compatible HTML.
///
/// Handles fenced code blocks, inline code, bold, italic, strikethrough,
/// links, headings, and blockquotes.  Text outside of code spans is
/// HTML-escaped so that `<`, `>`, and `&` don't break the parse.
///
/// Plain text without any markdown passes through unchanged (just escaped).
pub fn markdown_to_telegram_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let lines: Vec<&str> = input.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        // --- Fenced code blocks: ```lang ... ``` ---
        if line.trim_start().starts_with("```") {
            i += 1; // skip opening fence
            out.push_str("<pre>");
            while i < lines.len() {
                if lines[i].trim_start().starts_with("```") {
                    i += 1; // skip closing fence
                    break;
                }
                if !out.ends_with("<pre>") {
                    out.push('\n');
                }
                out.push_str(&escape_html(lines[i]));
                i += 1;
            }
            out.push_str("</pre>");
            out.push('\n');
            continue;
        }

        // --- Headings: # ... → <b>...</b> ---
        if let Some(rest) = strip_heading_prefix(line) {
            out.push_str("<b>");
            out.push_str(&convert_inline(&escape_html(rest)));
            out.push_str("</b>");
            out.push('\n');
            i += 1;
            continue;
        }

        // --- Blockquote: > ... ---
        if let Some(rest) = line.strip_prefix("> ").or_else(|| line.strip_prefix(">")) {
            out.push_str("<blockquote>");
            out.push_str(&convert_inline(&escape_html(rest)));
            out.push_str("</blockquote>");
            out.push('\n');
            i += 1;
            continue;
        }

        // --- Horizontal rule: --- / *** / ___ ---
        let trimmed = line.trim();
        if trimmed.len() >= 3
            && (trimmed.chars().all(|c| c == '-')
                || trimmed.chars().all(|c| c == '*')
                || trimmed.chars().all(|c| c == '_'))
        {
            out.push('\n');
            i += 1;
            continue;
        }

        // --- Regular line: escape HTML, then convert inline markdown ---
        out.push_str(&convert_inline(&escape_html(line)));
        out.push('\n');
        i += 1;
    }

    // Remove trailing newline added by our line-by-line processing.
    if out.ends_with('\n') {
        out.pop();
    }

    out
}

/// Escape `&`, `<`, and `>` for Telegram HTML.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Strip markdown heading prefix (`# `, `## `, etc.) and return the rest.
fn strip_heading_prefix(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let after_hashes = trimmed.trim_start_matches('#');
    after_hashes.strip_prefix(' ')
}

/// Convert inline markdown (bold, italic, strikethrough, code, links)
/// within an already HTML-escaped string.
fn convert_inline(s: &str) -> String {
    let s = convert_inline_code(s);
    let s = convert_links(&s);
    let s = convert_pattern(&s, "**", "<b>", "</b>");
    let s = convert_pattern(&s, "__", "<b>", "</b>");
    let s = convert_pattern(&s, "~~", "<s>", "</s>");
    let s = convert_pattern(&s, "*", "<i>", "</i>");
    convert_pattern(&s, "_", "<i>", "</i>")
}

/// Convert `` `inline code` `` spans.
fn convert_inline_code(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(start) = rest.find('`') {
        out.push_str(&rest[..start]);
        rest = &rest[start + 1..];
        if let Some(end) = rest.find('`') {
            out.push_str("<code>");
            out.push_str(&rest[..end]);
            out.push_str("</code>");
            rest = &rest[end + 1..];
        } else {
            out.push('`');
        }
    }
    out.push_str(rest);
    out
}

/// Convert `[text](url)` markdown links to `<a href="url">text</a>`.
fn convert_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(bracket_start) = rest.find('[') {
        if bracket_start > 0 && rest.as_bytes()[bracket_start - 1] == b'!' {
            out.push_str(&rest[..bracket_start + 1]);
            rest = &rest[bracket_start + 1..];
            continue;
        }
        out.push_str(&rest[..bracket_start]);
        rest = &rest[bracket_start + 1..];

        if let Some(bracket_end) = rest.find(']') {
            let link_text = &rest[..bracket_end];
            let after_bracket = &rest[bracket_end + 1..];

            if after_bracket.starts_with('(')
                && let Some(paren_end) = after_bracket.find(')')
            {
                let url = &after_bracket[1..paren_end];
                let raw_url = url
                    .replace("&amp;", "&")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">");
                out.push_str(&format!("<a href=\"{}\">{}</a>", raw_url, link_text));
                rest = &after_bracket[paren_end + 1..];
                continue;
            }

            out.push('[');
            out.push_str(link_text);
            rest = &rest[bracket_end..];
        } else {
            out.push('[');
        }
    }
    out.push_str(rest);
    out
}

/// Convert a symmetric two-char or one-char markdown pattern to HTML tags.
fn convert_pattern(s: &str, marker: &str, open: &str, close: &str) -> String {
    let mlen = marker.len();
    let mut out = String::with_capacity(s.len());
    let mut pos = 0;

    while pos < s.len() {
        let rest = &s[pos..];

        if rest.starts_with("<code>")
            && let Some(end) = rest.find("</code>")
        {
            out.push_str(&rest[..end + 7]);
            pos += end + 7;
            continue;
        }
        if rest.starts_with("<pre>")
            && let Some(end) = rest.find("</pre>")
        {
            out.push_str(&rest[..end + 6]);
            pos += end + 6;
            continue;
        }
        if rest.starts_with("<a ")
            && let Some(end) = rest.find("</a>")
        {
            out.push_str(&rest[..end + 4]);
            pos += end + 4;
            continue;
        }

        if rest.starts_with(marker) {
            if mlen == 1 {
                let prev_char = s[..pos].chars().next_back();
                if prev_char.is_some_and(|c| c.is_ascii_alphanumeric()) {
                    out.push_str(marker);
                    pos += mlen;
                    continue;
                }
            }

            let after_open = &s[pos + mlen..];
            if let Some(end_offset) = after_open.find(marker) {
                let inner = &after_open[..end_offset];
                let after = pos + mlen + end_offset + mlen;
                if mlen == 1 {
                    let next_char = s[after..].chars().next();
                    if next_char.is_some_and(|c| c.is_ascii_alphanumeric()) {
                        out.push_str(marker);
                        pos += mlen;
                        continue;
                    }
                }
                if !inner.is_empty() {
                    out.push_str(open);
                    out.push_str(inner);
                    out.push_str(close);
                    pos = after;
                    continue;
                }
            }
        }

        let ch = rest.chars().next().unwrap();
        out.push(ch);
        pos += ch.len_utf8();
    }

    out
}

/// Format log output for Telegram: HTML-escape, wrap in `<pre>` tags, and
/// split to respect the message length limit.
pub fn format_logs_for_telegram(logs: &str) -> Vec<String> {
    if logs.is_empty() {
        return vec!["No log output.".to_string()];
    }
    let max_content = MAX_MESSAGE_LEN - 11; // "<pre>".len() + "</pre>".len()
    let escaped = escape_html(logs);
    let parts = split_for_telegram_at(&escaped, max_content);
    parts
        .into_iter()
        .map(|p| format!("<pre>{p}</pre>"))
        .collect()
}

/// Strip bot @mentions from message text.
///
/// For commands (`/logs@botname 10` → `/logs 10`): strips the `@botname`
/// suffix that Telegram appends to commands in groups.
///
/// For regular messages (`@botname what's the weather?` → `what's the weather?`):
/// strips the `@botname` mention so the agent sees clean text.  Only removes
/// the mention matching `bot_username` (case-insensitive).
pub fn strip_bot_mention(text: &str, bot_username: &str) -> String {
    // Commands: strip @anything between the command and its arguments.
    if let Some(at) = text.find('@')
        && text.starts_with('/')
    {
        let after = text[at..].find(' ')
            .map(|sp| &text[at + sp..])
            .unwrap_or("");
        return format!("{}{}", &text[..at], after);
    }

    // Regular messages: strip @botname (case-insensitive, word-boundary aware).
    if !bot_username.is_empty() {
        let target = format!("@{bot_username}");
        let lower = text.to_lowercase();
        if let Some(pos) = lower.find(&target) {
            let after = pos + target.len();
            let at_boundary = after >= lower.len()
                || !lower.as_bytes()[after].is_ascii_alphanumeric()
                    && lower.as_bytes()[after] != b'_';
            if at_boundary {
                let mut result = String::with_capacity(text.len());
                result.push_str(&text[..pos]);
                if after < text.len() {
                    result.push_str(&text[after..]);
                }
                return result.trim().to_string();
            }
        }
    }

    text.to_string()
}

pub fn is_public_command(text: &str) -> bool {
    text == "/whoami"
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{classify_llm_error, LlmErrorKind};

    #[test]
    fn plain_text_passthrough() {
        assert_eq!(markdown_to_telegram_html("Hello world"), "Hello world");
    }

    #[test]
    fn plain_text_html_escaped() {
        assert_eq!(
            markdown_to_telegram_html("a < b & c > d"),
            "a &lt; b &amp; c &gt; d"
        );
    }

    #[test]
    fn bold() {
        assert_eq!(
            markdown_to_telegram_html("this is **bold** text"),
            "this is <b>bold</b> text"
        );
    }

    #[test]
    fn bold_underscore() {
        assert_eq!(
            markdown_to_telegram_html("this is __bold__ text"),
            "this is <b>bold</b> text"
        );
    }

    #[test]
    fn italic() {
        assert_eq!(
            markdown_to_telegram_html("this is *italic* text"),
            "this is <i>italic</i> text"
        );
    }

    #[test]
    fn strikethrough() {
        assert_eq!(
            markdown_to_telegram_html("this is ~~deleted~~ text"),
            "this is <s>deleted</s> text"
        );
    }

    #[test]
    fn inline_code() {
        assert_eq!(
            markdown_to_telegram_html("use `foo()` here"),
            "use <code>foo()</code> here"
        );
    }

    #[test]
    fn fenced_code_block() {
        let input = "before\n```rust\nfn main() {}\n```\nafter";
        let expected = "before\n<pre>fn main() {}</pre>\nafter";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn fenced_code_block_escapes_html() {
        let input = "```\na < b\n```";
        assert_eq!(markdown_to_telegram_html(input), "<pre>a &lt; b</pre>");
    }

    #[test]
    fn link() {
        assert_eq!(
            markdown_to_telegram_html("click [here](https://example.com)"),
            "click <a href=\"https://example.com\">here</a>"
        );
    }

    #[test]
    fn heading() {
        assert_eq!(markdown_to_telegram_html("# Title"), "<b>Title</b>");
        assert_eq!(markdown_to_telegram_html("## Subtitle"), "<b>Subtitle</b>");
        assert_eq!(markdown_to_telegram_html("### Deep"), "<b>Deep</b>");
    }

    #[test]
    fn blockquote() {
        assert_eq!(
            markdown_to_telegram_html("> quoted text"),
            "<blockquote>quoted text</blockquote>"
        );
    }

    #[test]
    fn horizontal_rule() {
        assert_eq!(markdown_to_telegram_html("---"), "");
        assert_eq!(markdown_to_telegram_html("***"), "");
        assert_eq!(markdown_to_telegram_html("___"), "");
    }

    #[test]
    fn combined_formatting() {
        assert_eq!(
            markdown_to_telegram_html("**bold** and *italic*"),
            "<b>bold</b> and <i>italic</i>"
        );
    }

    #[test]
    fn unclosed_backtick_kept() {
        assert_eq!(markdown_to_telegram_html("use `foo here"), "use `foo here");
    }

    #[test]
    fn empty_string() {
        assert_eq!(markdown_to_telegram_html(""), "");
    }

    #[test]
    fn mid_word_underscores_preserved() {
        assert_eq!(markdown_to_telegram_html("some_var_name"), "some_var_name");
    }

    #[test]
    fn code_content_not_formatted() {
        assert_eq!(
            markdown_to_telegram_html("`**not bold**`"),
            "<code>**not bold**</code>"
        );
    }

    #[test]
    fn multiline_message() {
        let input = "# Summary\n\nHello **world**.\n\n- item one\n- item two";
        let expected = "<b>Summary</b>\n\nHello <b>world</b>.\n\n- item one\n- item two";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn link_with_ampersand() {
        assert_eq!(
            markdown_to_telegram_html("[search](https://example.com?a=1&b=2)"),
            "<a href=\"https://example.com?a=1&b=2\">search</a>"
        );
    }

    #[test]
    fn multibyte_utf8_with_formatting() {
        assert_eq!(
            markdown_to_telegram_html("**pts/0** – your current shell"),
            "<b>pts/0</b> – your current shell"
        );
    }

    #[test]
    fn multibyte_utf8_emoji() {
        assert_eq!(
            markdown_to_telegram_html("hello **world** 🌍"),
            "hello <b>world</b> 🌍"
        );
    }

    #[test]
    fn classify_vision_openrouter_404() {
        assert!(matches!(
            classify_llm_error(
                "OpenAI API returned 404 Not Found: {\"error\":{\"message\":\"No endpoints found that support image input\",\"code\":404}}"
            ),
            LlmErrorKind::NoVision,
        ));
    }

    #[test]
    fn classify_vision_image_input() {
        assert!(matches!(
            classify_llm_error("does not support image input"),
            LlmErrorKind::NoVision,
        ));
    }

    #[test]
    fn classify_vision_keyword() {
        assert!(matches!(
            classify_llm_error("model does not support vision capabilities"),
            LlmErrorKind::NoVision,
        ));
    }

    #[test]
    fn classify_tool_use_error() {
        assert!(matches!(
            classify_llm_error(
                "OpenAI API returned 404 Not Found: {\"error\":{\"message\":\"No endpoints found that support tool use\",\"code\":404}}"
            ),
            LlmErrorKind::NoToolUse,
        ));
    }

    #[test]
    fn classify_tool_use_underscore() {
        assert!(matches!(
            classify_llm_error("model does not support tool_use"),
            LlmErrorKind::NoToolUse,
        ));
    }

    #[test]
    fn classify_unrelated_errors() {
        assert!(matches!(classify_llm_error("rate limit exceeded"), LlmErrorKind::Other));
        assert!(matches!(classify_llm_error("invalid API key"), LlmErrorKind::Other));
        assert!(matches!(classify_llm_error("context length exceeded"), LlmErrorKind::Other));
    }

    #[test]
    fn strip_bot_mention_plain_command() {
        assert_eq!(strip_bot_mention("/logs", "dysonbot"), "/logs");
    }

    #[test]
    fn strip_bot_mention_with_botname() {
        assert_eq!(strip_bot_mention("/logs@dysonbot", "dysonbot"), "/logs");
    }

    #[test]
    fn strip_bot_mention_with_botname_and_args() {
        assert_eq!(strip_bot_mention("/logs@dysonbot 10", "dysonbot"), "/logs 10");
    }

    #[test]
    fn strip_bot_mention_plain_with_args() {
        assert_eq!(strip_bot_mention("/logs 10", "dysonbot"), "/logs 10");
    }

    #[test]
    fn strip_bot_mention_whoami() {
        assert_eq!(strip_bot_mention("/whoami@mybot", "mybot"), "/whoami");
    }

    #[test]
    fn logs_command_with_botname_parses_line_count() {
        let input = "/logs@mybot 3";
        let normalized = strip_bot_mention(input, "mybot");
        assert_eq!(normalized, "/logs 3");
        let n: usize = normalized
            .strip_prefix("/logs")
            .unwrap()
            .trim()
            .parse()
            .unwrap_or(20);
        assert_eq!(n, 3);
    }

    #[test]
    fn strip_bot_mention_regular_message() {
        assert_eq!(
            strip_bot_mention("@dysonbot what's the weather?", "dysonbot"),
            "what's the weather?"
        );
    }

    #[test]
    fn strip_bot_mention_regular_message_mid_text() {
        assert_eq!(
            strip_bot_mention("hey @dysonbot what's up", "dysonbot"),
            "hey  what's up"
        );
    }

    #[test]
    fn strip_bot_mention_no_match_longer_username() {
        // Should NOT strip @dysonbot_extra since it's a different username.
        assert_eq!(
            strip_bot_mention("@dysonbot_extra hello", "dysonbot"),
            "@dysonbot_extra hello"
        );
    }

    #[test]
    fn strip_bot_mention_case_insensitive() {
        assert_eq!(
            strip_bot_mention("@DysonBot hello", "dysonbot"),
            "hello"
        );
    }

    #[test]
    fn strip_bot_mention_no_username() {
        assert_eq!(
            strip_bot_mention("@dysonbot hello", ""),
            "@dysonbot hello"
        );
    }

    #[test]
    fn format_logs_html_escapes_and_wraps_pre() {
        let logs = "2026-04-04 INFO something <weird> & good";
        let parts = format_logs_for_telegram(logs);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].starts_with("<pre>"));
        assert!(parts[0].ends_with("</pre>"));
        assert!(parts[0].contains("&lt;weird&gt;"));
        assert!(parts[0].contains("&amp;"));
    }

    #[test]
    fn format_logs_empty_returns_fallback() {
        let parts = format_logs_for_telegram("");
        assert_eq!(parts, vec!["No log output."]);
    }

    #[test]
    fn format_logs_long_output_splits_with_pre_tags() {
        let long_line = "x".repeat(3990);
        let logs = format!("{}\n{}", long_line, long_line);
        let parts = format_logs_for_telegram(&logs);
        assert!(parts.len() > 1);
        for part in &parts {
            assert!(part.starts_with("<pre>"), "part should start with <pre>");
            assert!(part.ends_with("</pre>"), "part should end with </pre>");
        }
    }
}
