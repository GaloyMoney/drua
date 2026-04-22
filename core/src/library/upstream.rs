use std::path::Path;

use super::LibraryError;

pub(super) async fn clone(repo_url: &str, repo_path: &Path) -> Result<(), LibraryError> {
    tracing::info!(url = %repo_url, path = %repo_path.display(), "cloning library repo");
    let output = tokio::process::Command::new("git")
        .args(["clone", repo_url, &repo_path.to_string_lossy()])
        .output()
        .await
        .map_err(|e| LibraryError::Git(format!("git clone: {e}")))?;
    if !output.status.success() {
        return Err(LibraryError::Git(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    tracing::info!(path = %repo_path.display(), "library repo cloned");
    Ok(())
}

pub(super) async fn add_and_commit(
    repo_path: &Path,
    relative_path: &str,
    message: &str,
) -> Result<(), LibraryError> {
    let add = tokio::process::Command::new("git")
        .args(["add", "--", relative_path])
        .current_dir(repo_path)
        .output()
        .await
        .map_err(|e| LibraryError::Git(format!("git add: {e}")))?;
    if !add.status.success() {
        return Err(LibraryError::Git(format!(
            "git add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        )));
    }

    let commit = tokio::process::Command::new("git")
        .args([
            "-c",
            "user.name=drua",
            "-c",
            "user.email=drua@galoy.io",
            "commit",
            "-m",
            message,
            "--",
            relative_path,
        ])
        .current_dir(repo_path)
        .output()
        .await
        .map_err(|e| LibraryError::Git(format!("git commit: {e}")))?;
    if !commit.status.success() {
        return Err(LibraryError::Git(format!(
            "git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr)
        )));
    }

    tracing::info!(path = relative_path, "committed runtime file");
    Ok(())
}

pub(super) async fn push_if_ahead(repo_path: &Path) -> Result<(), LibraryError> {
    let status = tokio::process::Command::new("git")
        .args(["status", "--porcelain", "-b"])
        .current_dir(repo_path)
        .output()
        .await
        .map_err(|e| LibraryError::Git(format!("git status: {e}")))?;
    let stdout = String::from_utf8_lossy(&status.stdout);
    if !stdout.contains("ahead") {
        return Ok(());
    }

    let push = tokio::process::Command::new("git")
        .args(["push"])
        .current_dir(repo_path)
        .output()
        .await
        .map_err(|e| LibraryError::Git(format!("git push: {e}")))?;
    if !push.status.success() {
        return Err(LibraryError::Git(format!(
            "git push failed: {}",
            String::from_utf8_lossy(&push.stderr)
        )));
    }

    tracing::info!("pushed runtime commits");
    Ok(())
}
