//! Push-the-mirror-to-upstream helper. After `git http-backend`
//! accepts a `git-receive-pack` POST and writes the new objects +
//! refs into the bare mirror, the proxy needs to mirror those refs
//! to github.com so the world sees them.
//!
//! `git2::Remote::push` with credential callbacks fed by the
//! per-push token from `UpstreamCredentialProvider`. Force-push is
//! implicit if the wire updates were force-pushes — git's pack
//! protocol carries the directive byte-for-byte.

use std::path::Path;

use crate::mirror::{MirrorError, UpstreamCredentialProvider};
use crate::pktline::RefUpdate;

/// Forwards every ref in `updates` from `mirror_path` to `upstream_url`.
/// Skipped if `updates` is empty (shouldn't happen — caller guards).
///
/// Refspecs use the leading `+` so non-fast-forward updates propagate
/// when the wire-level update was already non-ff (the upstream server
/// makes the final call on whether to accept).
pub async fn push_to_upstream(
    mirror_path: &Path,
    upstream_url: &str,
    updates: &[RefUpdate],
    creds: &dyn UpstreamCredentialProvider,
) -> Result<(), MirrorError> {
    if updates.is_empty() {
        return Ok(());
    }
    let token = creds.fresh_token().await?;
    let url = upstream_url.to_string();
    let mirror_path = mirror_path.to_path_buf();
    let refspecs: Vec<String> = updates
        .iter()
        .map(|u| format!("+{}:{}", u.ref_name, u.ref_name))
        .collect();

    tokio::task::spawn_blocking(move || -> Result<(), MirrorError> {
        let repo = git2::Repository::open_bare(&mirror_path)?;
        let mut remote = repo
            .remote_anonymous(&url)
            .or_else(|_| repo.remote_anonymous(&url))?;
        let mut callbacks = git2::RemoteCallbacks::new();
        callbacks.credentials(move |_url, _username, _allowed| {
            git2::Cred::userpass_plaintext("x-access-token", &token)
        });
        let mut push_opts = git2::PushOptions::new();
        push_opts.remote_callbacks(callbacks);
        let refspec_strs: Vec<&str> = refspecs.iter().map(String::as_str).collect();
        remote.push(&refspec_strs, Some(&mut push_opts))?;
        Ok(())
    })
    .await
    .map_err(|e| MirrorError::CredentialProvider(format!("join: {e}")))?
}
