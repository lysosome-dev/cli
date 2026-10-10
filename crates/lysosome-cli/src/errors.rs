//! Errors as the user sees them: a message, a stable machine-readable code,
//! and a hint that says how to fix it. With `--json` the same thing is
//! printed on stdout as `{"error": {...}}` (schema/error.schema.json).

use std::fmt;

use lysosome_core::Error as CoreError;
use serde_json::json;

/// A CLI error with a code and a fix. Build with [`fail`]; anything else
/// that reaches `main` is mapped by [`describe`].
#[derive(Debug, Clone)]
pub struct CliError {
    pub code: &'static str,
    pub message: String,
    pub hint: Option<String>,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

/// `Err(CliError)` as an `anyhow::Error`.
pub fn fail(
    code: &'static str,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> anyhow::Error {
    CliError {
        code,
        message: message.into(),
        hint: Some(hint.into()),
    }
    .into()
}

/// Code, message and hint for any error that reached `main`: the first
/// error in the chain we know how to explain decides.
pub fn describe(e: &anyhow::Error) -> CliError {
    for cause in e.chain() {
        if let Some(c) = cause.downcast_ref::<CliError>() {
            return c.clone();
        }
        if let Some(core) = cause.downcast_ref::<CoreError>() {
            return CliError {
                code: core.code(),
                message: format!("{e:#}"),
                hint: Some(core.hint().into()),
            };
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return CliError {
                code: "io",
                message: format!("{e:#}"),
                hint: Some(io_hint(io)),
            };
        }
    }
    CliError {
        code: "internal",
        message: format!("{e:#}"),
        hint: Some(
            "this looks like a bug; please report it at https://github.com/lysosome-dev/cli/issues"
                .into(),
        ),
    }
}

fn io_hint(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            "check the folder's permissions (on macOS, Full Disk Access for your terminal may be needed)".into()
        }
        std::io::ErrorKind::NotFound => "check that the path exists".into(),
        _ => "check the path and try again".into(),
    }
}

/// The `--json` error document (schema/error.schema.json).
pub fn json(e: &CliError) -> String {
    let doc = json!({
        "schema_version": lysosome_core::SCHEMA_VERSION,
        "tool_version": lysosome_core::TOOL_VERSION,
        "error": {
            "code": e.code,
            "message": e.message,
            "hint": e.hint,
        }
    });
    let mut s = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{\"error\":{}}".into());
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_gets_a_code_and_a_hint() {
        let cases: Vec<anyhow::Error> = vec![
            CoreError::NoValidRoots(vec!["/nope".into()]).into(),
            CoreError::NoHomeDir.into(),
            CoreError::Shared(std::sync::Arc::new(CoreError::NoValidRoots(vec![]))).into(),
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "x").into(),
            anyhow::Error::from(std::io::Error::new(std::io::ErrorKind::NotFound, "y"))
                .context("reading a thing"),
            fail("needs_yes", "m", "h"),
            anyhow::anyhow!("something odd"),
        ];
        let codes: Vec<&str> = cases
            .iter()
            .map(|e| {
                let d = describe(e);
                assert!(d.hint.as_deref().is_some_and(|h| !h.is_empty()), "{d:?}");
                d.code
            })
            .collect();
        assert_eq!(
            codes,
            [
                "no_valid_roots",
                "no_home_dir",
                "no_valid_roots",
                "io",
                "io",
                "needs_yes",
                "internal"
            ]
        );
        let doc: serde_json::Value = serde_json::from_str(&json(&describe(&cases[0]))).unwrap();
        assert_eq!(doc["error"]["code"], "no_valid_roots");
        assert!(doc["error"]["message"].as_str().unwrap().contains("/nope"));
    }
}
