//! Utility functions for `VelaClaw`.
//!
//! This module contains reusable helper functions used across the codebase.

use std::cmp;

/// Truncate at or before `byte_idx` to the last UTF-8 character boundary (MSRV: `&str` before 1.91).
#[must_use]
pub fn floor_char_boundary(s: &str, byte_idx: usize) -> usize {
    let mut i = cmp::min(byte_idx, s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Truncate a string to at most `max_chars` characters, appending "..." if truncated.
///
/// This function safely handles multi-byte UTF-8 characters (emoji, CJK, accented characters)
/// by using character boundaries instead of byte indices.
///
/// # Arguments
/// * `s` - The string to truncate
/// * `max_chars` - Maximum number of characters to keep (excluding "...")
///
/// # Returns
/// * Original string if length <= `max_chars`
/// * Truncated string with "..." appended if length > `max_chars`
///
/// # Examples
/// ```ignore
/// use velaclaw::util::truncate_with_ellipsis;
///
/// // ASCII string - no truncation needed
/// assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
///
/// // ASCII string - truncation needed
/// assert_eq!(truncate_with_ellipsis("hello world", 5), "hello...");
///
/// // Multi-byte UTF-8 (emoji) - safe truncation
/// assert_eq!(truncate_with_ellipsis("Hello 🦀 World", 8), "Hello 🦀...");
/// assert_eq!(truncate_with_ellipsis("😀😀😀😀", 2), "😀😀...");
///
/// // Empty string
/// assert_eq!(truncate_with_ellipsis("", 10), "");
/// ```
pub fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => {
            let truncated = &s[..idx];
            // Trim trailing whitespace for cleaner output
            format!("{}...", truncated.trim_end())
        }
        None => s.to_string(),
    }
}

/// DeepSeek DSML delimiter (U+FF5C), same set as `strip_tool_call_markup`.
pub(crate) const DSML_TAG: &str = "\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}";

/// Open tags stripped from user-visible text; admit uses the same list.
pub(crate) const TOOL_CALL_OPEN_TAGS: [&str; 9] = [
    "<function_calls>",
    "<function_call>",
    "<tool_call>",
    "<toolcall>",
    "<tool-call>",
    "<tool_request>",
    "<tool>",
    "<invoke>",
    "<$call>",
];

/// True when `s` still contains a tool-call carrier (DSML delimiter or open tag).
#[must_use]
pub fn invocation_contains_carrier(s: &str) -> bool {
    s.contains(DSML_TAG) || TOOL_CALL_OPEN_TAGS.iter().any(|tag| s.contains(tag))
}

/// Strip internal tool-invocation markup from user-visible agent text.
///
/// Removes `<tool_call>`, `<tool_request>`, DSML wrappers, and related dialect
/// tags so CLI/channel/Web UI users never see raw protocol scaffolding.
#[must_use]
pub fn strip_tool_call_markup(message: &str) -> String {
    fn find_first_tag<'a>(haystack: &str, tags: &'a [&'a str]) -> Option<(usize, &'a str)> {
        tags.iter()
            .filter_map(|tag| haystack.find(tag).map(|idx| (idx, *tag)))
            .min_by_key(|(idx, _)| *idx)
    }

    fn matching_close_tag(open_tag: &str) -> Option<&'static str> {
        match open_tag {
            "<function_calls>" => Some("</function_calls>"),
            "<function_call>" => Some("</function_call>"),
            "<tool_call>" => Some("</tool_call>"),
            "<toolcall>" => Some("</toolcall>"),
            "<tool-call>" => Some("</tool-call>"),
            "<tool_request>" => Some("</tool_request>"),
            "<tool>" => Some("</tool>"),
            "<invoke>" => Some("</invoke>"),
            "<$call>" => Some("</$call>"),
            _ => None,
        }
    }

    fn extract_first_json_end(input: &str) -> Option<usize> {
        let trimmed = input.trim_start();
        let trim_offset = input.len().saturating_sub(trimmed.len());

        for (byte_idx, ch) in trimmed.char_indices() {
            if ch != '{' && ch != '[' {
                continue;
            }

            let slice = &trimmed[byte_idx..];
            let mut stream =
                serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
            if let Some(Ok(_value)) = stream.next() {
                let consumed = stream.byte_offset();
                if consumed > 0 {
                    return Some(trim_offset + byte_idx + consumed);
                }
            }
        }

        None
    }

    fn strip_leading_close_tags(mut input: &str) -> &str {
        loop {
            let trimmed = input.trim_start();
            if !trimmed.starts_with("</") {
                return trimmed;
            }

            let Some(close_end) = trimmed.find('>') else {
                return "";
            };
            input = &trimmed[close_end + 1..];
        }
    }

    /// Strip DeepSeek DSML wrappers (`<｜｜DSML｜｜tool_call>` / `_call` / bare / …).
    fn strip_dsml_blocks(message: &str) -> String {
        if !message.contains(DSML_TAG) {
            return message.to_string();
        }
        // Any DSML-delimited element (tool_call, _call, invoke, parameter, bare).
        // Open/close suffixes may disagree; treat the U+FF5C family as one unit.
        let re = regex::Regex::new(&format!(r"(?s)<{DSML_TAG}[^>]*>.*?</{DSML_TAG}[^>]*>"))
            .expect("valid dsml strip regex");
        let stripped = re.replace_all(message, "");
        // Unclosed hybrid: open tag + JSON body, then optional mismatched close.
        let open_re =
            regex::Regex::new(&format!(r"(?s)<{DSML_TAG}[^>]*>")).expect("valid dsml open regex");
        let mut out = String::new();
        let mut rest = stripped.as_ref();
        while let Some(m) = open_re.find(rest) {
            out.push_str(&rest[..m.start()]);
            let after = &rest[m.end()..];
            if let Some(json_end) = extract_first_json_end(after) {
                rest = strip_leading_close_tags(&after[json_end..]);
            } else {
                // Drop from open tag to end — better than leaking DSML to the user.
                rest = "";
                break;
            }
        }
        out.push_str(rest);
        // Drop orphan DSML open/close tags left by mismatched closes.
        let orphan =
            regex::Regex::new(&format!(r"</?{DSML_TAG}[^>]*>")).expect("valid orphan dsml regex");
        orphan.replace_all(&out, "").trim().to_string()
    }

    let message = strip_dsml_blocks(message);
    let mut kept_segments = Vec::new();
    let mut remaining = message.as_str();

    while let Some((start, open_tag)) = find_first_tag(remaining, &TOOL_CALL_OPEN_TAGS) {
        let before = &remaining[..start];
        if !before.is_empty() {
            kept_segments.push(before.to_string());
        }

        let Some(close_tag) = matching_close_tag(open_tag) else {
            break;
        };
        let after_open = &remaining[start + open_tag.len()..];

        if let Some(close_idx) = after_open.find(close_tag) {
            remaining = &after_open[close_idx + close_tag.len()..];
            continue;
        }

        if let Some(consumed_end) = extract_first_json_end(after_open) {
            remaining = strip_leading_close_tags(&after_open[consumed_end..]);
            continue;
        }

        kept_segments.push(remaining[start..].to_string());
        remaining = "";
        break;
    }

    if !remaining.is_empty() {
        kept_segments.push(remaining.to_string());
    }

    let mut result = kept_segments.concat();

    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }

    result.trim().to_string()
}

/// Utility enum for handling optional values.
pub enum MaybeSet<T> {
    Set(T),
    Unset,
    Null,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_ascii_no_truncation() {
        // ASCII string shorter than limit - no change
        assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
        assert_eq!(truncate_with_ellipsis("hello world", 50), "hello world");
    }

    #[test]
    fn test_truncate_ascii_with_truncation() {
        // ASCII string longer than limit - truncates
        assert_eq!(truncate_with_ellipsis("hello world", 5), "hello...");
        assert_eq!(
            truncate_with_ellipsis("This is a long message", 10),
            "This is a..."
        );
    }

    #[test]
    fn test_truncate_empty_string() {
        assert_eq!(truncate_with_ellipsis("", 10), "");
    }

    #[test]
    fn test_truncate_at_exact_boundary() {
        // String exactly at boundary - no truncation
        assert_eq!(truncate_with_ellipsis("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_emoji_single() {
        // Single emoji (4 bytes) - should not panic
        let s = "🦀";
        assert_eq!(truncate_with_ellipsis(s, 10), s);
        assert_eq!(truncate_with_ellipsis(s, 1), s);
    }

    #[test]
    fn test_truncate_emoji_multiple() {
        // Multiple emoji - safe truncation at character boundary
        let s = "😀😀😀😀"; // 4 emoji, each 4 bytes = 16 bytes total
        assert_eq!(truncate_with_ellipsis(s, 2), "😀😀...");
        assert_eq!(truncate_with_ellipsis(s, 3), "😀😀😀...");
    }

    #[test]
    fn test_truncate_mixed_ascii_emoji() {
        // Mixed ASCII and emoji
        assert_eq!(truncate_with_ellipsis("Hello 🦀 World", 8), "Hello 🦀...");
        assert_eq!(truncate_with_ellipsis("Hi 😊", 10), "Hi 😊");
    }

    #[test]
    fn test_truncate_cjk_characters() {
        // CJK characters (Chinese - each is 3 bytes)
        let s = "这是一个测试消息用来触发崩溃的中文"; // 21 characters
        let result = truncate_with_ellipsis(s, 16);
        assert!(result.ends_with("..."));
        assert!(result.is_char_boundary(result.len() - 1));
    }

    #[test]
    fn test_truncate_accented_characters() {
        // Accented characters (2 bytes each in UTF-8)
        let s = "café résumé naïve";
        assert_eq!(truncate_with_ellipsis(s, 10), "café résum...");
    }

    #[test]
    fn test_truncate_unicode_edge_case() {
        // Mix of 1-byte, 2-byte, 3-byte, and 4-byte characters
        let s = "aé你好🦀"; // 1 + 1 + 2 + 2 + 4 bytes = 10 bytes, 5 chars
        assert_eq!(truncate_with_ellipsis(s, 3), "aé你...");
    }

    #[test]
    fn test_truncate_long_string() {
        // Long ASCII string
        let s = "a".repeat(200);
        let result = truncate_with_ellipsis(&s, 50);
        assert_eq!(result.len(), 53); // 50 + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_truncate_zero_max_chars() {
        // Edge case: max_chars = 0
        assert_eq!(truncate_with_ellipsis("hello", 0), "...");
    }

    #[test]
    fn strip_tool_call_markup_removes_tags() {
        let input = "Hi\n<tool_call>\n{\"name\":\"shell\"}\n</tool_call>\nBye";
        let out = strip_tool_call_markup(input);
        assert!(!out.contains("<tool_call>"));
        assert!(out.contains("Hi"));
        assert!(out.contains("Bye"));
    }

    #[test]
    fn strip_tool_call_markup_removes_tool_request() {
        let input = "<tool_request>\n{\"name\":\"shell\"}\n</tool_request>";
        assert_eq!(strip_tool_call_markup(input), "");
    }

    #[test]
    fn strip_tool_call_markup_removes_mismatched_dsml_hybrid() {
        let tag = "\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}";
        let input = format!(
            "需要了解 obvs。\n<{tag}tool_call>\n\
             {{\"name\": \"shell\", \"arguments\": {{\"command\": \"ls\"}}}}\n\
             </{tag}tool_calls>"
        );
        let out = strip_tool_call_markup(&input);
        assert_eq!(out, "需要了解 obvs。");
        assert!(!out.contains("DSML"));
        assert!(!out.contains("tool_call"));
    }

    #[test]
    fn strip_tool_call_markup_removes_dsml_parameter_mismatch_close() {
        // DeepSeek V4 wire: open tool_call, flat args, close parameter then tool_call.
        let tag = "\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}";
        let input = format!(
            "<{tag}tool_call>\n\
             {{\"name\": \"shell\", \"command\": \"ssh piubt true\"}}\n\
             </{tag}parameter>\n\
             </{tag}tool_call>"
        );
        let out = strip_tool_call_markup(&input);
        assert!(!out.contains("DSML"), "out={out:?}");
        assert!(!out.contains("tool_call"), "out={out:?}");
        assert!(!out.contains("shell"), "out={out:?}");
    }

    #[test]
    fn strip_tool_call_markup_removes_dsml_underscore_call() {
        let tag = "\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}";
        let input = format!(
            "先读配置：\n<{tag}_call>\n\
             {{\"name\": \"shell\", \"arguments\": {{\"command\": \"ssh x\"}}}}\n\
             </{tag}_call>"
        );
        let out = strip_tool_call_markup(&input);
        assert_eq!(out, "先读配置：");
        assert!(!out.contains("DSML"), "out={out:?}");
        assert!(!out.contains("_call"), "out={out:?}");
    }

    #[test]
    fn strip_tool_call_markup_removes_mixed_standard_dsml_close() {
        let tag = "\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}";
        let input =
            format!("<tool_call>\n{{\"name\": \"shell\", \"command\": \"ssh x\"}}\n</{tag}>");
        let out = strip_tool_call_markup(&input);
        assert!(out.is_empty(), "out={out:?}");
    }

    #[test]
    fn invocation_contains_carrier_matches_strip_tag_set() {
        assert!(invocation_contains_carrier("<tool_call>"));
        assert!(invocation_contains_carrier(DSML_TAG));
        assert!(!invocation_contains_carrier("cat README.md"));
    }
}
