//! No lease authority off Unix: its trust checks (ownership, modes,
//! process groups) are Unix's, so elsewhere none is ever configured.

use std::path::Path;

use mdkg_format::Diagnostic;
use mdkg_graph::GraphError;
use serde::de::DeserializeOwned;

use crate::coord::Repo;
use crate::output::Report;

#[derive(Debug)]
pub(crate) enum Authority {}

pub(crate) fn find(_repo: &Repo) -> Result<Option<Authority>, GraphError> {
    Ok(None)
}

impl Authority {
    pub(crate) fn name(&self) -> &str {
        match *self {}
    }

    pub(crate) fn delegate<T: DeserializeOwned>(
        &self,
        _root: &Path,
        _command: &str,
        _args: &[String],
    ) -> Result<Report<T>, GraphError> {
        match *self {}
    }
}

pub(crate) fn committed_key_warning(_root: &Path) -> Option<Diagnostic> {
    None
}
