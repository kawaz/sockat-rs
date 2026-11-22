//! The `--json` output of `resolve` (docs/design/cli.md "出力"): one object,
//! the members in the order the design shows them.

use std::fmt::Write;
use std::path::Path;

use sockat::__cli as entry;

use crate::resolve::{Failure, Resolved};

/// The object for `resolved`, made from `original` (the path as given).
///
/// # Errors
///
/// When a path is not UTF-8: JSON has no way to write it, and a path changed
/// to fit would not be the path.
pub(crate) fn resolved(original: &Path, resolved: &Resolved) -> Result<String, Failure> {
    let mut members: Vec<(&str, String)> = vec![
        ("path", string(&resolved.path)?),
        ("bytes", resolved.path.as_os_str().len().to_string()),
        ("limit", entry::LIMIT.to_string()),
        ("original", string(original)?),
        ("original_bytes", original.as_os_str().len().to_string()),
        ("route", quote(resolved.route.name())),
    ];
    if let Some((link, target)) = &resolved.link {
        members.push(("link", string(link)?));
        members.push(("target", string(target)?));
    }
    let uses = match resolved.bind {
        true => "[\"bind\", \"connect\"]",
        false => "[\"connect\"]",
    };
    members.push(("for", uses.to_owned()));
    // Always there, empty when there is nothing to warn of (cli.md "出力").
    let warnings: Vec<String> = resolved.warnings.iter().map(|w| quote(w)).collect();
    members.push(("warnings", format!("[{}]", warnings.join(", "))));
    let mut out = String::from("{\n");
    for (i, (key, value)) in members.iter().enumerate() {
        let comma = if i + 1 < members.len() { "," } else { "" };
        let _ = writeln!(out, "  {}: {value}{comma}", quote(key));
    }
    out.push_str("}\n");
    Ok(out)
}

/// `p` as a JSON string.
fn string(p: &Path) -> Result<String, Failure> {
    match p.to_str() {
        Some(s) => Ok(quote(s)),
        None => Err(Failure {
            message: format!(
                "the path is not UTF-8, which JSON cannot hold: {}\n  run without --json to get the path alone",
                p.display()
            ),
            usage: false,
        }),
    }
}

/// `s` as a JSON string: `"` and `\` escaped, and the control characters.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 || c == '\u{7f}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
