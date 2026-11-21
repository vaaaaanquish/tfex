//! Line-oriented reading of native-syntax files the parser rejected.
//!
//! A file with a syntax error should still be searchable, so its top-level
//! blocks are found with regular expressions after comments are masked. This
//! understands quoted strings but neither heredocs nor nesting, and finds no
//! references between declarations, which is why it is only the fallback.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::{
    Backend, Configuration, LineIndex, ModuleCall, Resource, ResourceKind, is_safe_backend_setting,
};

static BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r#"(?m)^[ \t]*(resource|data|module|provider)\s+"([^"\n]+)"(?:\s+"([^"\n]+)")?\s*\{"#)
});
static BACKEND: LazyLock<Regex> =
    LazyLock::new(|| pattern(r#"(?m)^[ \t]*backend\s+"([^"\n]+)"\s*\{"#));
static ARGUMENT: LazyLock<Regex> =
    LazyLock::new(|| pattern(r#"(?m)^[ \t]*([a-zA-Z_][\w-]*)\s*=\s*"([^"\n]*)""#));

#[expect(
    clippy::expect_used,
    reason = "the patterns are constants covered by the tests"
)]
fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("valid pattern")
}

/// Add the declarations found in `text` to `configuration`.
pub(super) fn read(text: &str, file: &str, configuration: &mut Configuration) {
    let masked = mask_comments(text);
    let lines = LineIndex::new(&masked);
    for found in BLOCK.captures_iter(&masked) {
        let header = found.get_match();
        let line = lines.line_at(header.start());
        let opening_brace = header.end() - 1;
        let first_label = &found[2];
        let second_label = found.get(3).map(|label| label.as_str());
        match (&found[1], second_label) {
            ("resource", Some(name)) => configuration.resources.push(Resource::new(
                ResourceKind::Managed,
                first_label,
                name,
                file,
                line,
            )),
            ("data", Some(name)) => configuration.resources.push(Resource::new(
                ResourceKind::Data,
                first_label,
                name,
                file,
                line,
            )),
            ("provider", _) => configuration.has_provider = true,
            ("module", _) => configuration.module_calls.push(ModuleCall::new(
                first_label,
                string_arguments(&masked, opening_brace).remove("source"),
                file,
                line,
            )),
            _ => {}
        }
    }
    if let Some(found) = BACKEND.captures(&masked) {
        let opening_brace = found.get_match().end() - 1;
        configuration.backend = Some(Backend {
            kind: found[1].to_owned(),
            settings: string_arguments(&masked, opening_brace)
                .into_iter()
                .filter(|(name, _)| is_safe_backend_setting(name))
                .map(|(name, value)| (name, Value::String(value)))
                .collect(),
        });
    }
}

/// The `name = "value"` arguments in the block opened by the brace at
/// `opening_brace`, at any depth.
fn string_arguments(masked: &str, opening_brace: usize) -> BTreeMap<String, String> {
    let closing_brace = matching_brace(masked, opening_brace);
    ARGUMENT
        .captures_iter(&masked[opening_brace + 1..closing_brace])
        .map(|found| (found[1].to_owned(), found[2].to_owned()))
        .collect()
}

/// Replace comments with spaces, so that byte offsets and line numbers of the
/// remaining text are unchanged.
fn mask_comments(text: &str) -> String {
    let source = text.as_bytes();
    let mut masked = source.to_vec();
    let mut offset = 0;
    while offset < source.len() {
        let rest = &source[offset..];
        let length = if rest[0] == b'"' {
            quoted_length(rest)
        } else if rest.starts_with(b"/*") {
            let length = block_comment_length(rest);
            blank(&mut masked[offset..offset + length]);
            length
        } else if rest[0] == b'#' || rest.starts_with(b"//") {
            let length = line_comment_length(rest);
            blank(&mut masked[offset..offset + length]);
            length
        } else {
            1
        };
        offset += length;
    }
    // Comments were overwritten byte for byte with ASCII spaces, and their
    // delimiters are ASCII, so no multi-byte character was split.
    String::from_utf8(masked).unwrap_or_else(|_| text.to_owned())
}

/// Overwrite everything except newlines with spaces.
fn blank(bytes: &mut [u8]) {
    for byte in bytes.iter_mut().filter(|byte| **byte != b'\n') {
        *byte = b' ';
    }
}

/// Length of the quoted string at the start of `text`, quotes included. An
/// unterminated string extends to the end of the text.
fn quoted_length(text: &[u8]) -> usize {
    let mut offset = 1;
    while offset < text.len() {
        match text[offset] {
            // An escaped character, such as `\"`, cannot end the string.
            b'\\' => offset += 2,
            b'"' => return offset + 1,
            _ => offset += 1,
        }
    }
    text.len()
}

/// Length of the `/* ... */` comment at the start of `text`.
fn block_comment_length(text: &[u8]) -> usize {
    const DELIMITER: usize = 2;
    text[DELIMITER..]
        .windows(DELIMITER)
        .position(|window| window == b"*/")
        .map_or(text.len(), |position| DELIMITER + position + DELIMITER)
}

/// Length of the `#` or `//` comment at the start of `text`, which runs up to
/// but not including the end of the line.
fn line_comment_length(text: &[u8]) -> usize {
    text.iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(text.len())
}

/// Offset of the `}` that closes the `{` at `opening_brace`, or the end of
/// the text when the block is never closed. Braces in strings do not count.
fn matching_brace(text: &str, opening_brace: usize) -> usize {
    let source = text.as_bytes();
    let mut depth = 0_usize;
    let mut offset = opening_brace;
    while offset < source.len() {
        match source[offset] {
            b'"' => {
                offset += quoted_length(&source[offset..]);
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return offset;
                }
            }
            _ => {}
        }
        offset += 1;
    }
    source.len()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_a_file_with_a_syntax_error() {
        let text = r#"# resource "a" "commented" {}
resource "aws_s3_bucket" "logs" {
  bucket = "x"
/* resource "a" "blocked" {} */
module "network" {
  source = "./modules/network" // trailing
}
terraform {
  backend "gcs" {
    bucket      = "state"
    credentials = "SECRET"
  }
}
data "aws_region" "current" {}
provider "google" {
"#;
        let mut configuration = Configuration::default();
        read(text, "main.tf", &mut configuration);

        let declared: Vec<(String, usize)> = configuration
            .resources
            .iter()
            .map(|resource| (resource.address(), resource.line))
            .collect();
        assert_eq!(
            declared,
            [
                ("aws_s3_bucket.logs".to_owned(), 2),
                ("data.aws_region.current".to_owned(), 14)
            ]
        );
        assert_eq!(
            configuration.module_calls,
            [ModuleCall::new(
                "network",
                Some("./modules/network".to_owned()),
                "main.tf",
                5
            )]
        );
        assert!(configuration.has_provider);
        let backend = configuration.backend.unwrap();
        assert_eq!(backend.kind, "gcs");
        assert_eq!(json!(backend.settings), json!({"bucket": "state"}));
    }

    #[test]
    fn masking_keeps_offsets_lines_and_strings() {
        let text = "a = \"# not // a comment\" # コメント\n/* x\ny */ b = 1 // z\n";
        let masked = mask_comments(text);
        assert_eq!(masked.len(), text.len());
        assert_eq!(
            masked,
            format!(
                "a = \"# not // a comment\" {}\n{}\n{} b = 1 {}\n",
                " ".repeat("# コメント".len()),
                " ".repeat(4),
                " ".repeat(4),
                " ".repeat(4)
            )
        );
    }

    #[test]
    fn masking_honors_escaped_quotes() {
        let text = r#"a = "say \"hi\" # still a string" # comment"#;
        let masked = mask_comments(text);
        assert!(masked.contains(r#""say \"hi\" # still a string""#));
        assert!(!masked.contains("comment"));
    }

    #[test]
    fn matching_brace_skips_nesting_and_strings() {
        let text = r#"{ a { b = "}" } c = "\"}" } tail }"#;
        assert_eq!(matching_brace(text, 0), text.find(" tail").unwrap() - 1);
        assert_eq!(matching_brace("{ never closed", 0), "{ never closed".len());
    }
}
