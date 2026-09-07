//! Transient dependency errors — the one distinction the consensus loop needs
//! in order to stop killing itself.
//!
//! An execution client that is briefly AWAY (restarting: ~10 s snapshot
//! replay on the lean node) or BEHIND (an EL answering `SYNCING`) is not a
//! consensus failure; it is "no verdict right now". A handler that propagates
//! a plain `Err` to the consensus loop turns that into process death, and the
//! docker restart then re-boots into the same outage and parks the validator
//! permanently (2026-08-26/27: every fleet wedge of the campaign traced to
//! this one reflex — and the EVM lane only escapes it because its EL is
//! co-located, IPC-attached and never restarted). Errors carrying this marker
//! are safe to answer with "skip this round / re-request later" instead.
use std::fmt;

/// A dependency (execution engine, lean lane node) that did not answer
/// usefully *right now*. Classified by [`is_transient`] via downcast — never
/// by matching message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransientDependencyError {
    /// Which dependency: "execution engine" or "lean lane node".
    pub dependency: &'static str,
    /// Human-readable detail. Also the full `Display` text, so the existing
    /// message-asserting tests keep their exact expectations.
    pub detail: String,
}

impl TransientDependencyError {
    pub fn new(dependency: &'static str, detail: impl Into<String>) -> Self {
        Self { dependency, detail: detail.into() }
    }
}

impl fmt::Display for TransientDependencyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for TransientDependencyError {}

/// True iff any error in the report's chain is a [`TransientDependencyError`]
/// — it survives every `wrap_err` layer the handlers add on the way up.
pub fn is_transient(report: &eyre::Report) -> bool {
    report
        .chain()
        .any(|e| e.downcast_ref::<TransientDependencyError>().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eyre::{eyre, WrapErr};

    #[test]
    fn marker_survives_wrap_err_layers() {
        let base: eyre::Report =
            TransientDependencyError::new("lean lane node", "gone").into();
        let wrapped = base
            .wrap_err("lean lane: node unreachable during validation")
            .wrap_err("Payload validation failed on block built from synced value");
        assert!(is_transient(&wrapped));
        assert!(wrapped.to_string().contains("synced value"));
    }

    #[test]
    fn plain_errors_are_not_transient() {
        let e = eyre!("engine down").wrap_err("validation failed");
        assert!(!is_transient(&e));
    }

    #[test]
    fn display_is_the_detail() {
        let e = TransientDependencyError::new("execution engine", "unexpected SYNCING");
        assert_eq!(e.to_string(), "unexpected SYNCING");
    }
}
