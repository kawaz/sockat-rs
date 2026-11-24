//! The `--json` output of `resolve`, `bind` and `connect`
//! (docs/design/cli.md "出力"): one object, the members in the order the
//! design shows them.

use std::fmt::Write;
use std::path::Path;

use sockat::__cli as entry;
use sockat::{Spelling, Until};

use crate::resolve::{Failure, Resolved};
use crate::trial::Outcome;

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
    Ok(object(&members))
}

/// The object for what `bind` or `connect` came to: the members of the
/// conformance CLI's output (`ok`, `error`, `route`, `base`, `spelling`,
/// with its values) and the command's own (`skipped` with the reasons,
/// `kernel_path`, `left`).
///
/// # Errors
///
/// When a path is not UTF-8, as for [`resolved`].
pub(crate) fn outcome(o: &Outcome<'_>) -> Result<String, Failure> {
    let or_null = |v: Option<String>| v.unwrap_or_else(|| "null".to_owned());
    let skipped: Vec<String> = o
        .skipped
        .iter()
        .map(|s| {
            format!(
                "{{ \"name\": {}, \"reason\": {} }}",
                quote(s.name),
                quote(s.reason)
            )
        })
        .collect();
    let left = o
        .left
        .iter()
        .map(|(e, _)| string(e))
        .collect::<Result<Vec<_>, _>>()?;
    let members: Vec<(&str, String)> = vec![
        ("ok", o.error.is_none().to_string()),
        (
            "error",
            or_null(o.error.map(|e| quote(&entry::error_name(e)))),
        ),
        ("route", or_null(o.route.map(quote))),
        ("base", or_null(o.base.map(quote))),
        ("spelling", or_null(o.spelling.map(spelling))),
        ("skipped", format!("[{}]", skipped.join(", "))),
        (
            "kernel_path",
            or_null(o.kernel_path.as_deref().map(string).transpose()?),
        ),
        ("left", format!("[{}]", left.join(", "))),
    ];
    Ok(object(&members))
}

/// `s` as the conformance CLI writes it: `kind`, and for `Linked` its
/// attributes under their Rust names.
fn spelling(s: Spelling) -> String {
    match s {
        Spelling::Linked {
            removable,
            until,
            usable_cross_uid,
            usable_with_private_tmp,
            ..
        } => format!(
            "{{ \"kind\": \"Linked\", \"removable\": {removable}, \"until\": {}, \"usable_cross_uid\": {usable_cross_uid}, \"usable_with_private_tmp\": {usable_with_private_tmp} }}",
            quote(match until {
                Until::Process => "Process",
                Until::Cleanup => "Cleanup",
            })
        ),
        s => format!(
            "{{ \"kind\": {} }}",
            quote(match s {
                Spelling::Original => "Original",
                Spelling::CwdRelative => "CwdRelative",
                Spelling::Dangling => "Dangling",
                _ => "Unknown",
            })
        ),
    }
}

/// One member a line, in the order of `members`.
fn object(members: &[(&str, String)]) -> String {
    let mut out = String::from("{\n");
    for (i, (key, value)) in members.iter().enumerate() {
        let comma = if i + 1 < members.len() { "," } else { "" };
        let _ = writeln!(out, "  {}: {value}{comma}", quote(key));
    }
    out.push_str("}\n");
    out
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
