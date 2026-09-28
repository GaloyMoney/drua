//! Filesystem facade over the server-side library clone, scoped to
//! `space:<slug>/...` paths. Used by the top-level file tools (Read,
//! LS, Glob, Grep, Edit, Move, Delete) to dispatch space-rooted ops
//! without needing an attached sandbox.
//!
//! All reads go through the bare clone via libgit2 — no on-disk
//! materialisation, no `tokio::fs`, no `rg` subprocess. The path
//! model is purely relative (`<rel_path>` inside `spaces/<slug>/`);
//! callers never see absolute filesystem paths.
//!
//! Every public method takes a raw `path: &str` and returns
//! `Result<Option<T>, ProjectError>`. `Ok(None)` signals "not a `space:`
//! path — fall through to the existing sandbox dispatch"; `Ok(Some(_))`
//! is a successful space-rooted op. Authorization runs once in
//! `resolve` via `Projects::space_for_subject`, which gates project
//! agents on the mount and lets admins reach any space; per-method
//! docs only call out method-specific behaviour.

use std::sync::Arc;

use tracing::instrument;

use drua_library::{BlobEntries, PathDates, PathDatesMap, Space, SpaceError, Spaces};

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::changeset::{Changeset, ChangesetError, ChangesetStatus, Changesets};
use crate::primitives::ChangesetId;
use crate::project::{ProjectError, Projects};
use crate::user::Users;

/// Soft cap on `view_file` to keep tool responses bounded. Larger than
/// the typical sandbox `text_editor view` ceiling; agents needing more
/// should `grep` first.
const MAX_VIEW_FILE_BYTES: usize = 1_048_576; // 1 MiB

/// Result of `view_file` — files return their content; directory paths
/// return their listing so callers can format consistently.
pub enum FileView {
    File(String),
    Dir(Vec<String>),
}

/// One `LS`/`Glob` entry alongside its git-derived dates, for `details:
/// true` callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailedEntry {
    /// Same string `view_dir` / `glob` would have returned (directories
    /// keep their trailing `/`).
    pub entry: String,
    /// `None` for directories and for a path with no commit history.
    pub dates: Option<PathDates>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpaceScheme {
    Space,
    Draft,
}

impl SpaceScheme {
    fn prefix(self) -> &'static str {
        match self {
            SpaceScheme::Space => "space",
            SpaceScheme::Draft => "draft",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SpaceRef<'a> {
    scheme: SpaceScheme,
    slug: &'a str,
    /// Empty for the space root (`space:<slug>` or `space:<slug>/`).
    rel_path: &'a str,
    changeset_id: Option<ChangesetId>,
}

fn parse_space_path(path: &str) -> Option<SpaceRef<'_>> {
    let (scheme, rest) = if let Some(rest) = path.strip_prefix("draft:") {
        (SpaceScheme::Draft, rest)
    } else {
        (SpaceScheme::Space, path.strip_prefix("space:")?)
    };
    let (slug_part, rel) = match rest.split_once('/') {
        Some((slug_part, rel)) => (slug_part, rel),
        None => (rest, ""),
    };
    let (slug, changeset_id) = match slug_part.split_once('@') {
        Some((slug, raw_id)) => (slug, Some(raw_id.parse::<ChangesetId>().ok()?)),
        None => (slug_part, None),
    };
    if slug.is_empty() {
        return None;
    }
    Some(SpaceRef {
        scheme,
        slug,
        rel_path: rel,
        changeset_id,
    })
}

fn is_bare_space_path(path: &str) -> bool {
    let rest = path
        .strip_prefix("space:")
        .or_else(|| path.strip_prefix("draft:"));
    let Some(rest) = rest else {
        return false;
    };
    rest.trim_matches('/').is_empty()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Main,
    Changeset {
        id: ChangesetId,
        handle: drua_library::DraftHandle,
        status: ChangesetStatus,
        title: String,
        touched: usize,
        just_started: bool,
    },
}

impl Target {
    fn space_target(&self) -> drua_library::SpaceTarget {
        match self {
            Target::Main => drua_library::SpaceTarget::Main,
            Target::Changeset { handle, .. } => drua_library::SpaceTarget::Draft(handle.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    Read,
    Write,
}

struct Resolved {
    space: Space,
    /// Owned so the bundle outlives the input `&str`.
    rel_path: String,
    target: Target,
    stamp: String,
}

#[derive(Clone)]
pub struct SpaceFs {
    spaces: Arc<Spaces>,
    projects: Arc<Projects>,
    users: Arc<Users>,
    changesets: Arc<Changesets>,
}

impl SpaceFs {
    pub fn new(
        spaces: Arc<Spaces>,
        projects: Arc<Projects>,
        users: Arc<Users>,
        changesets: Arc<Changesets>,
    ) -> Self {
        Self {
            spaces,
            projects,
            users,
            changesets,
        }
    }

    /// Read complete bytes through ordinary space resolution and authorization.
    /// Non-space paths return `None`; missing space files return `PathNotFound`.
    #[instrument(name = "library.space_fs.read_file_bytes", skip(self, sub))]
    pub async fn read_file_bytes(
        &self,
        sub: &AuthSubject,
        path: &str,
    ) -> Result<Option<Vec<u8>>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path, Intent::Read).await? else {
            return Ok(None);
        };
        let bytes = self
            .spaces
            .read_file(
                &resolved.space.slug,
                &resolved.rel_path,
                &resolved.target.space_target(),
            )
            .await?
            .ok_or_else(|| SpaceError::PathNotFound {
                slug: resolved.space.slug.clone(),
                path: resolved.rel_path,
            })?;
        Ok(Some(bytes))
    }

    pub async fn resolved_stamp(
        &self,
        sub: &AuthSubject,
        path: &str,
        write: bool,
    ) -> Result<Option<String>, ProjectError> {
        let intent = if write { Intent::Write } else { Intent::Read };
        Ok(self.resolve(sub, path, intent).await?.map(|r| r.stamp))
    }

    pub fn is_space_path(path: &str) -> bool {
        let rest = path
            .strip_prefix("space:")
            .or_else(|| path.strip_prefix("draft:"));
        let Some(rest) = rest else {
            return false;
        };
        let slug = rest.split_once('/').map_or(rest, |(s, _)| s);
        !slug.is_empty()
    }

    async fn resolve(
        &self,
        sub: &AuthSubject,
        path: &str,
        intent: Intent,
    ) -> Result<Option<Resolved>, ProjectError> {
        let Some(mut sref) = parse_space_path(path) else {
            if path.starts_with("space:") || path.starts_with("draft:") {
                return Err(SpaceError::BadRequest {
                    reason: format!(
                        "'{path}' is not a valid space URI; expected 'space:<slug>', 'space:<slug>/<rel>', 'space:<slug>@<changeset-id>/<rel>', or 'draft:<slug>/<rel>'"
                    ),
                }
                .into());
            }
            return Ok(None);
        };
        if let (SpaceScheme::Draft, Some(id)) = (sref.scheme, sref.changeset_id) {
            return Err(SpaceError::BadRequest {
                reason: format!(
                    "'{path}' is not valid — 'draft:' always targets your own draft and never takes an '@<changeset-id>' override; use 'space:{}@{id}/...' to read a specific changeset",
                    sref.slug,
                ),
            }
            .into());
        }
        let typed_scheme = sref.scheme;
        let is_run_subject = sub.in_workflow_run();
        if is_run_subject {
            if sref.changeset_id.is_some() && intent == Intent::Write {
                return Err(SpaceError::BadRequest {
                    reason: format!(
                        "'{path}': a workflow run writes only its own draft; drop the '@<changeset-id>'"
                    ),
                }
                .into());
            }
            if sref.changeset_id.is_none() {
                sref.scheme = SpaceScheme::Draft;
            }
        }
        let in_run = is_run_subject && sref.changeset_id.is_none();
        let space = self.projects.space_for_subject(sub, sref.slug).await?;
        let rel_path = normalize_rel_path(sref.rel_path);
        Self::validate_rel_path(&rel_path)?;
        let target = self.resolve_target(sub, &space, &sref, intent).await?;
        let differs = self
            .differs_note(sub, &sref, &rel_path, &target, intent)
            .await;
        let stamp = stamp(
            typed_scheme,
            &space.slug,
            &target,
            differs.as_deref(),
            in_run,
        );
        Ok(Some(Resolved {
            space,
            rel_path,
            target,
            stamp,
        }))
    }

    async fn resolve_target(
        &self,
        sub: &AuthSubject,
        space: &Space,
        sref: &SpaceRef<'_>,
        intent: Intent,
    ) -> Result<Target, ProjectError> {
        if let Some(id) = sref.changeset_id {
            let cs = self
                .changesets
                .find_for_target(sub, id)
                .await
                .map_err(map_changeset_err)?;
            if intent == Intent::Write {
                if !sub.can_draft_spaces() {
                    return Err(SpaceError::ReadOnly {
                        slug: space.slug.clone(),
                    }
                    .into());
                }
                if !cs.is_open() {
                    return Err(SpaceError::ChangesetNotOpen {
                        id: cs.id.to_string(),
                        status: format!("{:?}", cs.status),
                    }
                    .into());
                }
            }
            return self.changeset_target(cs, false).await;
        }

        match sref.scheme {
            SpaceScheme::Draft => {
                if intent != Intent::Write {
                    if !sub.can_draft_spaces() {
                        return Err(SpaceError::ReadOnly {
                            slug: space.slug.clone(),
                        }
                        .into());
                    }
                    return match self
                        .changesets
                        .open_draft_for(sub)
                        .await
                        .map_err(map_changeset_err)?
                    {
                        Some(cs) => self.changeset_target(cs, false).await,
                        None => Ok(Target::Main),
                    };
                }
                if !sub.can_draft_spaces() {
                    return Err(SpaceError::ReadOnly {
                        slug: space.slug.clone(),
                    }
                    .into());
                }
                match self
                    .changesets
                    .open_draft_for(sub)
                    .await
                    .map_err(map_changeset_err)?
                {
                    Some(cs) => self.changeset_target(cs, false).await,
                    None if sub.in_workflow_run() => Err(SpaceError::RunReadOnly {
                        slug: space.slug.clone(),
                    }
                    .into()),
                    None => {
                        let cs = self
                            .changesets
                            .draft_for(sub, None, None, Some(sref.rel_path))
                            .await
                            .map_err(map_changeset_err)?;
                        self.changeset_target(cs, true).await
                    }
                }
            }
            SpaceScheme::Space => {
                if intent != Intent::Write {
                    return Ok(Target::Main);
                }
                if !sub.can_draft_spaces() {
                    return Err(SpaceError::ReadOnly {
                        slug: space.slug.clone(),
                    }
                    .into());
                }
                match self
                    .changesets
                    .open_draft_for(sub)
                    .await
                    .map_err(map_changeset_err)?
                {
                    Some(cs) => Err(SpaceError::DraftOpen {
                        id: short_id(cs.id),
                        title: cs.title,
                        slug: space.slug.clone(),
                    }
                    .into()),
                    None => Ok(Target::Main),
                }
            }
        }
    }

    async fn changeset_target(
        &self,
        cs: Changeset,
        just_started: bool,
    ) -> Result<Target, ProjectError> {
        // ensure_ref first: it recovers from a not-yet-fetched base commit
        // (changeset::ensure_ref), so touched_count's diff runs once that
        // recovery has had a chance to bring the objects in.
        let handle = self
            .changesets
            .ensure_ref(&cs)
            .await
            .map_err(map_changeset_err)?;
        let touched = self.changesets.touched_count(&cs).await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, changeset_id = %cs.id, "changeset_target: touched_count failed; reporting 0");
            0
        });
        Ok(Target::Changeset {
            id: cs.id,
            handle,
            status: cs.status,
            title: cs.title,
            touched,
            just_started,
        })
    }

    async fn differs_note(
        &self,
        sub: &AuthSubject,
        sref: &SpaceRef<'_>,
        rel_path: &str,
        target: &Target,
        intent: Intent,
    ) -> Option<String> {
        if intent != Intent::Read
            || sref.scheme != SpaceScheme::Space
            || !matches!(target, Target::Main)
        {
            return None;
        }
        let draft = self.changesets.open_draft_for(sub).await.ok().flatten()?;
        let touched = self
            .changesets
            .touched_paths_in(&draft, sref.slug)
            .await
            .ok()?;
        touched
            .iter()
            .any(|p| p == rel_path)
            .then_some(short_id(draft.id))
    }

    /// Slugs of every space the subject can address — admins see all
    /// spaces in the library; project subjects see their project's
    /// `mounted_spaces`. Returned as `space:<slug>/` directory entries
    /// so the LS surface formats them like any other listing.
    #[instrument(name = "library.space_fs.list_mounted_spaces", skip(self, sub))]
    pub async fn list_mounted_spaces(
        &self,
        sub: &AuthSubject,
    ) -> Result<Vec<String>, ProjectError> {
        Audit::record_action_if_unset("space.list_mounted");
        let spaces = self.projects.list_visible_spaces(sub).await?;
        let mut entries: Vec<String> = spaces
            .into_iter()
            .map(|s| format!("space:{}/", s.slug))
            .collect();
        entries.sort();
        Ok(entries)
    }

    /// View a file (or list a directory) under a `space:` path. Reads
    /// go through `Spaces::read_file` / `Spaces::list_dir` — straight
    /// from the bare clone via libgit2, no on-disk materialisation.
    /// Applies the model-facing `MAX_VIEW_FILE_BYTES` cap; use
    /// [`Self::view_file_with_cap`] to lift it.
    #[instrument(name = "library.space_fs.view_file", skip(self, sub))]
    pub async fn view_file(
        &self,
        sub: &AuthSubject,
        path: &str,
        view_range: Option<(i64, i64)>,
    ) -> Result<Option<FileView>, ProjectError> {
        self.view_file_with_cap(sub, path, view_range, Some(MAX_VIEW_FILE_BYTES))
            .await
    }

    /// `view_file` with an explicit byte cap. `cap: None` lifts
    /// `MAX_VIEW_FILE_BYTES` entirely — for a compose script, which has
    /// its own, larger guard (`toolsets.compose.max_tool_result_bytes`)
    /// instead of the model-facing one.
    #[instrument(name = "library.space_fs.view_file_with_cap", skip(self, sub))]
    pub async fn view_file_with_cap(
        &self,
        sub: &AuthSubject,
        path: &str,
        view_range: Option<(i64, i64)>,
        cap: Option<usize>,
    ) -> Result<Option<FileView>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path, Intent::Read).await? else {
            return Ok(None);
        };
        let target = resolved.target.space_target();

        // Try as a directory first; if it's a tree, list it. If not a
        // tree, fall through to a blob read.
        if let Some(entries) = self
            .spaces
            .list_dir(&resolved.space.slug, &resolved.rel_path, &target)
            .await
            .map_err(|e| -> ProjectError { e.into() })?
        {
            return Ok(Some(FileView::Dir(format_dir(entries))));
        }

        let bytes = self
            .spaces
            .read_file(&resolved.space.slug, &resolved.rel_path, &target)
            .await
            .map_err(|e| -> ProjectError { e.into() })?
            .ok_or_else(|| io_err(format!("no such file: {}", resolved.rel_path)))?;
        let content = text_from_bytes(bytes, cap)?;
        Ok(Some(FileView::File(apply_view_range(&content, view_range))))
    }

    /// Bare `space:` returns mounted-space slugs (see `list_mounted_spaces`).
    /// Paths resolving to no entries return an empty listing (not an Io error) so post-delete LS doesn't pollute audit error counts.
    #[instrument(name = "library.space_fs.view_dir", skip(self, sub))]
    pub async fn view_dir(
        &self,
        sub: &AuthSubject,
        path: &str,
    ) -> Result<Option<Vec<String>>, ProjectError> {
        if is_bare_space_path(path) {
            return Ok(Some(self.list_mounted_spaces(sub).await?));
        }
        let Some(resolved) = self.resolve(sub, path, Intent::Read).await? else {
            return Ok(None);
        };
        let entries = self
            .spaces
            .list_dir(
                &resolved.space.slug,
                &resolved.rel_path,
                &resolved.target.space_target(),
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?
            .unwrap_or_default();
        Ok(Some(format_dir(entries)))
    }

    /// `view_dir` with each file's `created`/`modified` attached. The
    /// bare `space:` listing has no dates — mounted slugs come back
    /// with `dates: None` rather than an error.
    #[instrument(name = "library.space_fs.view_dir_detailed", skip(self, sub))]
    pub async fn view_dir_detailed(
        &self,
        sub: &AuthSubject,
        path: &str,
    ) -> Result<Option<Vec<DetailedEntry>>, ProjectError> {
        if is_bare_space_path(path) {
            let mounted = self.list_mounted_spaces(sub).await?;
            return Ok(Some(
                mounted
                    .into_iter()
                    .map(|entry| DetailedEntry { entry, dates: None })
                    .collect(),
            ));
        }
        let Some(resolved) = self.resolve(sub, path, Intent::Read).await? else {
            return Ok(None);
        };
        let entries = self
            .spaces
            .list_dir(
                &resolved.space.slug,
                &resolved.rel_path,
                &resolved.target.space_target(),
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?
            .unwrap_or_default();
        let dates = self
            .spaces
            .path_dates(&resolved.space.slug)
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        let rel_path = resolved.rel_path;
        Ok(Some(join_dates(
            format_dir(entries),
            dates.as_deref(),
            |name| {
                if rel_path.is_empty() {
                    name.to_string()
                } else {
                    format!("{rel_path}/{name}")
                }
            },
        )))
    }

    #[instrument(name = "library.space_fs.write_file", skip(self, sub, content))]
    pub async fn write_file(
        &self,
        sub: &AuthSubject,
        path: &str,
        content: String,
    ) -> Result<Option<String>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path, Intent::Write).await? else {
            return Ok(None);
        };
        Audit::record_action_if_unset("space.write_file");
        Self::record_changeset_audit(&resolved.target);
        let attribution = self.users.commit_attribution().await;
        let oid = self
            .spaces
            .write_file(
                &resolved.space.slug,
                &resolved.rel_path,
                content,
                attribution,
                &resolved.target.space_target(),
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        self.record_write(&resolved.target, oid, "write_file", &resolved.rel_path)
            .await?;
        Ok(Some(self.stamp_after_write(sub, path, &resolved).await?))
    }

    /// `text_editor` `str_replace`. The unique-occurrence check happens
    /// at the worker (`Spaces::str_replace`) against the freshest disk
    /// state.
    #[instrument(
        name = "library.space_fs.str_replace",
        skip(self, sub, old_str, new_str)
    )]
    pub async fn str_replace(
        &self,
        sub: &AuthSubject,
        path: &str,
        old_str: String,
        new_str: String,
    ) -> Result<Option<String>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path, Intent::Write).await? else {
            return Ok(None);
        };
        Audit::record_action_if_unset("space.str_replace");
        Self::record_changeset_audit(&resolved.target);
        let attribution = self.users.commit_attribution().await;
        let oid = self
            .spaces
            .str_replace(
                &resolved.space.slug,
                &resolved.rel_path,
                old_str,
                new_str,
                attribution,
                &resolved.target.space_target(),
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        self.record_write(&resolved.target, oid, "str_replace", &resolved.rel_path)
            .await?;
        Ok(Some(self.stamp_after_write(sub, path, &resolved).await?))
    }

    /// `text_editor` `insert`. Insertion happens at the worker against
    /// the freshest disk state.
    #[instrument(name = "library.space_fs.insert_line", skip(self, sub, text))]
    pub async fn insert_line(
        &self,
        sub: &AuthSubject,
        path: &str,
        line_number: usize,
        text: String,
    ) -> Result<Option<String>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path, Intent::Write).await? else {
            return Ok(None);
        };
        Audit::record_action_if_unset("space.insert");
        Self::record_changeset_audit(&resolved.target);
        let attribution = self.users.commit_attribution().await;
        let oid = self
            .spaces
            .insert(
                &resolved.space.slug,
                &resolved.rel_path,
                line_number,
                text,
                attribution,
                &resolved.target.space_target(),
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        self.record_write(&resolved.target, oid, "insert", &resolved.rel_path)
            .await?;
        Ok(Some(self.stamp_after_write(sub, path, &resolved).await?))
    }

    /// Removes the file at `space:<slug>/<rel>`. Success even if the
    /// file was already gone.
    #[instrument(name = "library.space_fs.delete_file", skip(self, sub))]
    pub async fn delete_file(
        &self,
        sub: &AuthSubject,
        path: &str,
    ) -> Result<Option<String>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path, Intent::Write).await? else {
            return Ok(None);
        };
        Audit::record_action_if_unset("space.delete_file");
        Self::record_changeset_audit(&resolved.target);
        let attribution = self.users.commit_attribution().await;
        let oid = self
            .spaces
            .delete_file(
                &resolved.space.slug,
                &resolved.rel_path,
                attribution,
                &resolved.target.space_target(),
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        self.record_write(&resolved.target, oid, "delete_file", &resolved.rel_path)
            .await?;
        Ok(Some(self.stamp_after_write(sub, path, &resolved).await?))
    }

    /// Renames `from` → `to` within a single space. `Ok(None)` only when
    /// _both_ paths are sandbox paths (caller falls through). Mixed
    /// space/sandbox or cross-space moves are hard errors so the agent
    /// gets a clear message instead of silent fall-through.
    #[instrument(name = "library.space_fs.move_file", skip(self, sub))]
    pub async fn move_file(
        &self,
        sub: &AuthSubject,
        from: &str,
        to: &str,
    ) -> Result<Option<String>, ProjectError> {
        let from_is_space = Self::is_space_path(from);
        let to_is_space = Self::is_space_path(to);
        if !from_is_space && !to_is_space {
            return Ok(None);
        }
        if from_is_space != to_is_space {
            return Err(SpaceError::CrossSpaceMove {
                from_slug: parse_space_path(from)
                    .map(|r| r.slug.to_string())
                    .unwrap_or_else(|| "<sandbox>".to_string()),
                to_slug: parse_space_path(to)
                    .map(|r| r.slug.to_string())
                    .unwrap_or_else(|| "<sandbox>".to_string()),
            }
            .into());
        }
        if let (Some(from_sref), Some(to_sref)) = (parse_space_path(from), parse_space_path(to)) {
            if from_sref.changeset_id.is_some() != to_sref.changeset_id.is_some() {
                return Err(SpaceError::BadRequest {
                    reason: format!(
                        "'{from}' -> '{to}': an explicit '@<changeset-id>' must appear on both sides of a move, or neither"
                    ),
                }
                .into());
            }
            if from_sref.scheme != to_sref.scheme {
                return Err(SpaceError::BadRequest {
                    reason: format!(
                        "'{from}' -> '{to}': both sides of a move must use the same scheme ('space:' or 'draft:')"
                    ),
                }
                .into());
            }
        }
        let Some(from_resolved) = self.resolve(sub, from, Intent::Write).await? else {
            return Ok(None);
        };
        let Some(to_ref) = parse_space_path(to) else {
            return Ok(None);
        };
        if from_resolved.space.slug != to_ref.slug {
            return Err(SpaceError::CrossSpaceMove {
                from_slug: from_resolved.space.slug.clone(),
                to_slug: to_ref.slug.to_string(),
            }
            .into());
        }
        let to_rel = normalize_rel_path(to_ref.rel_path);
        Self::validate_rel_path(&to_rel)?;
        Audit::record_action_if_unset("space.move_file");
        Self::record_changeset_audit(&from_resolved.target);
        let attribution = self.users.commit_attribution().await;
        let oid = self
            .spaces
            .move_file(
                &from_resolved.space.slug,
                &from_resolved.rel_path,
                &to_rel,
                attribution,
                &from_resolved.target.space_target(),
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        self.record_write(&from_resolved.target, oid, "move_file", &to_rel)
            .await?;
        Ok(Some(
            self.stamp_after_write(sub, from, &from_resolved).await?,
        ))
    }

    /// Glob walk across the space's tree. Pattern is the standard
    /// glob syntax (`*`, `**`, `?`), matched against each blob's path
    /// relative to `path`'s rel-component (the space root for
    /// `space:<slug>`), mirroring the sandbox `rg --files --glob`
    /// backend; results are `spaces/<slug>/`-relative. `path`'s
    /// rel-component anchors the search root — a directory, or a
    /// single file; naming neither is an error.
    #[instrument(name = "library.space_fs.glob", skip(self, sub))]
    pub async fn glob(
        &self,
        sub: &AuthSubject,
        path: &str,
        pattern: &str,
    ) -> Result<Option<Vec<String>>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path, Intent::Read).await? else {
            return Ok(None);
        };
        let blobs = self.walk_search_root(&resolved).await?;
        Ok(Some(glob_blobs(blobs, &resolved.rel_path, pattern)?))
    }

    /// `glob` with each match's `created`/`modified` attached. A glob
    /// result is already a `spaces/<slug>/`-relative path, so it *is*
    /// its own `PathDatesMap` key — no prefixing needed.
    #[instrument(name = "library.space_fs.glob_detailed", skip(self, sub))]
    pub async fn glob_detailed(
        &self,
        sub: &AuthSubject,
        path: &str,
        pattern: &str,
    ) -> Result<Option<Vec<DetailedEntry>>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path, Intent::Read).await? else {
            return Ok(None);
        };
        let blobs = self.walk_search_root(&resolved).await?;
        let files = glob_blobs(blobs, &resolved.rel_path, pattern)?;
        let dates = self
            .spaces
            .path_dates(&resolved.space.slug)
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        Ok(Some(join_dates(files, dates.as_deref(), |entry| {
            entry.to_string()
        })))
    }

    /// Grep walk across the space's tree. Replicates the curated subset
    /// of flags the `Grep` top-level tool exposes, but without `rg` —
    /// runs each blob's content through the `regex` crate. `path`
    /// anchors the search root — a directory, or a single file; naming
    /// neither is an error.
    #[instrument(name = "library.space_fs.grep", skip(self, sub, args))]
    pub async fn grep(
        &self,
        sub: &AuthSubject,
        path: &str,
        args: &sandbox::GrepInput,
    ) -> Result<Option<String>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path, Intent::Read).await? else {
            return Ok(None);
        };
        let blobs = self.walk_search_root(&resolved).await?;
        Ok(Some(grep_blobs(blobs, &resolved.rel_path, args)?))
    }

    /// Blobs under an already-resolved search root. A path that names
    /// nothing is an `Err`, never `Ok(None)` — post-`resolve`, `None`
    /// would reach the top-level `Grep`/`Glob` tools as "not a space
    /// path" and send `space:<slug>/...` on to the sandbox.
    async fn walk_search_root(&self, resolved: &Resolved) -> Result<BlobEntries, ProjectError> {
        match self
            .spaces
            .walk(
                &resolved.space.slug,
                &resolved.rel_path,
                &resolved.target.space_target(),
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?
        {
            Some(blobs) => Ok(blobs),
            // A space whose root has never been written has no tree yet;
            // an empty result is the honest answer there.
            None if resolved.rel_path.is_empty() => Ok(Vec::new()),
            None => Err(io_err(format!("no such file or directory: {}", resolved.rel_path)).into()),
        }
    }

    fn record_changeset_audit(target: &Target) {
        if let Target::Changeset { id, .. } = target {
            Audit::record_changeset_id(*id);
        }
    }

    async fn record_write(
        &self,
        target: &Target,
        oid: Option<String>,
        action: &str,
        path: &str,
    ) -> Result<(), ProjectError> {
        let Target::Changeset { id, .. } = target else {
            return Ok(());
        };
        let Some(head_oid) = oid else {
            return Ok(());
        };
        self.changesets
            .record_commit(*id, head_oid, action, path)
            .await
            .map_err(map_changeset_err)
    }

    async fn stamp_after_write(
        &self,
        sub: &AuthSubject,
        path: &str,
        resolved: &Resolved,
    ) -> Result<String, ProjectError> {
        let Target::Changeset {
            id,
            just_started: false,
            ..
        } = &resolved.target
        else {
            return Ok(resolved.stamp.clone());
        };
        let Some(sref) = parse_space_path(path) else {
            return Ok(resolved.stamp.clone());
        };
        let in_run = sub.in_workflow_run() && sref.changeset_id.is_none();
        let cs = self
            .changesets
            .find_for_target(sub, *id)
            .await
            .map_err(map_changeset_err)?;
        let target = self.changeset_target(cs, false).await?;
        Ok(stamp(
            sref.scheme,
            &resolved.space.slug,
            &target,
            None,
            in_run,
        ))
    }

    /// Rejects path-traversal, absolute paths, NUL bytes, and leading `/`.
    /// Empty `rel` is allowed (means "the space root").
    pub(crate) fn validate_rel_path(rel: &str) -> Result<(), SpaceError> {
        if rel.is_empty() {
            return Ok(());
        }
        if rel.contains('\0') || rel.starts_with('/') || rel.starts_with('\\') {
            return Err(invalid_rel_path(rel));
        }
        for segment in rel.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(invalid_rel_path(rel));
            }
        }
        Ok(())
    }
}

/// Strips `.` segments — the no-op CWD shorthand. Callers (`resolve`,
/// `move_file`) normalise before validation so agents can pass `.` /
/// `./foo` / `foo/./bar` interchangeably with `""` / `foo` / `foo/bar`.
/// Does NOT collapse `..` (real traversal); those still fail validation.
fn normalize_rel_path(rel: &str) -> String {
    rel.split('/')
        .filter(|seg| *seg != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// `path` relative to `root` (both `spaces/<slug>/`-relative), or
/// `None` when `path` doesn't live under `root` at all. `root` empty
/// means "the space root" — unchanged, so every root-anchored caller
/// keeps working. `path == root` means root names a single file, which
/// matches against its own basename (as `rg` does). Blobs outside
/// `root` are never produced by `walk_search_root` in practice; `None`
/// here is belt-and-suspenders so a `**` pattern still can't cross into
/// a sibling directory if that ever changes.
fn root_relative<'a>(root: &str, path: &'a str) -> Option<&'a str> {
    if root.is_empty() {
        return Some(path);
    }
    if path == root {
        return Some(path.rsplit('/').next().unwrap_or(path));
    }
    path.strip_prefix(root).and_then(|r| r.strip_prefix('/'))
}

/// Filter `blobs` (rel-path, bytes) by a glob pattern — matched against
/// each path relative to `root` — and return the matching paths (still
/// `spaces/<slug>/`-relative), sorted.
fn glob_blobs(
    blobs: Vec<(String, Vec<u8>)>,
    root: &str,
    pattern: &str,
) -> Result<Vec<String>, SpaceError> {
    let regex = glob_to_regex(pattern)
        .map_err(|e| io_err(format!("invalid glob pattern '{pattern}': {e}")))?;
    let mut out: Vec<String> = blobs
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| matches!(root_relative(root, p), Some(rel) if regex.is_match(rel)))
        .collect();
    out.sort();
    Ok(out)
}

/// Run `grep` over already-walked blobs. Mirrors the curated subset of
/// flags the `Grep` top-level tool accepts. `glob` filters are matched
/// against each path relative to `root`; printed match paths stay
/// `spaces/<slug>/`-relative.
fn grep_blobs(
    blobs: Vec<(String, Vec<u8>)>,
    root: &str,
    args: &sandbox::GrepInput,
) -> Result<String, SpaceError> {
    let mode = args.output_mode.unwrap_or_default();
    let case_insensitive = args.case_insensitive;
    let multiline = args.multiline;
    let glob_filter = args
        .glob
        .as_deref()
        .map(glob_to_regex)
        .transpose()
        .map_err(|e| io_err(format!("invalid glob filter: {e}")))?;
    let show_line_nums = args.line_numbers.unwrap_or(true);
    let context_after = args.after_context.unwrap_or(0) as usize;
    let context_before = args.before_context.unwrap_or(0) as usize;
    let context_around = args.context.unwrap_or(0) as usize;
    let head_limit = args.head_limit.map(|n| n as usize);

    let regex = regex::RegexBuilder::new(&args.pattern)
        .case_insensitive(case_insensitive)
        .multi_line(multiline)
        .dot_matches_new_line(multiline)
        .build()
        .map_err(|e| io_err(format!("invalid regex '{}': {e}", args.pattern)))?;

    let before = context_before.max(context_around);
    let after = context_after.max(context_around);

    let mut output_lines: Vec<String> = Vec::new();
    for (rel, bytes) in blobs {
        if let Some(g) = glob_filter.as_ref() {
            match root_relative(root, &rel) {
                Some(r) if g.is_match(r) => {}
                _ => continue,
            }
        }
        let Ok(content) = std::str::from_utf8(&bytes) else {
            continue;
        };

        match mode {
            sandbox::GrepOutputMode::FilesWithMatches => {
                if regex.is_match(content) {
                    output_lines.push(rel);
                }
            }
            sandbox::GrepOutputMode::Count => {
                let n = regex.find_iter(content).count();
                if n > 0 {
                    output_lines.push(format!("{rel}:{n}"));
                }
            }
            sandbox::GrepOutputMode::Content => {
                let lines: Vec<&str> = content.lines().collect();
                let mut matched_idx: Vec<usize> = Vec::new();
                for (i, line) in lines.iter().enumerate() {
                    if regex.is_match(line) {
                        matched_idx.push(i);
                    }
                }
                if matched_idx.is_empty() {
                    continue;
                }
                // Build the context-window line set (deduped).
                let mut want: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
                for i in &matched_idx {
                    let lo = i.saturating_sub(before);
                    let hi = (*i + after).min(lines.len().saturating_sub(1));
                    for j in lo..=hi {
                        want.insert(j);
                    }
                }
                for idx in want {
                    if show_line_nums {
                        output_lines.push(format!("{rel}:{}:{}", idx + 1, lines[idx]));
                    } else {
                        output_lines.push(format!("{rel}:{}", lines[idx]));
                    }
                }
            }
        }
    }

    if let Some(cap) = head_limit {
        output_lines.truncate(cap);
    }
    Ok(output_lines.join("\n"))
}

fn invalid_rel_path(rel: &str) -> SpaceError {
    SpaceError::InvalidRelPath {
        path: rel.to_string(),
        reason: "must be relative; no '..', leading '/', empty segments, or NUL bytes".into(),
    }
}

fn format_dir(entries: Vec<drua_library::DirEntry>) -> Vec<String> {
    entries
        .into_iter()
        .map(|e| {
            if e.is_dir {
                format!("{}/", e.name)
            } else {
                e.name
            }
        })
        .collect()
}

/// Joins entry strings (as `format_dir` / `glob_blobs` return them)
/// with their dates from `dates`, via `key_for` mapping a bare entry
/// to its `PathDatesMap` key. Directories (entries ending in `/`)
/// never get dates — D7: dates are a per-file git-history property, a
/// directory has none of its own — so `key_for` is never called for
/// one.
fn join_dates(
    entries: Vec<String>,
    dates: Option<&PathDatesMap>,
    key_for: impl Fn(&str) -> String,
) -> Vec<DetailedEntry> {
    entries
        .into_iter()
        .map(|entry| {
            let found = if entry.ends_with('/') {
                None
            } else {
                dates.and_then(|m| m.get(&key_for(&entry)).copied())
            };
            DetailedEntry {
                entry,
                dates: found,
            }
        })
        .collect()
}

fn short_id(id: ChangesetId) -> String {
    id.to_string().chars().take(8).collect()
}

fn stamp(
    scheme: SpaceScheme,
    slug: &str,
    target: &Target,
    differs: Option<&str>,
    in_run: bool,
) -> String {
    let prefix = scheme.prefix();
    match target {
        Target::Main if scheme == SpaceScheme::Draft => format!("[draft:{slug} · no draft]"),
        Target::Main => match differs {
            Some(id) => format!("[space:{slug} · main · differs in your draft {id}]"),
            None => format!("[space:{slug} · main]"),
        },
        Target::Changeset {
            id, status, title, ..
        } if scheme == SpaceScheme::Space && *status != ChangesetStatus::Open => {
            format!(
                "[space:{slug}@{} · changeset \"{title}\" · {status:?}]",
                short_id(*id)
            )
        }
        Target::Changeset {
            id, title, touched, ..
        } if in_run => format!(
            "[{prefix}:{slug} · run draft {} \"{title}\" · {touched} file{}]",
            short_id(*id),
            if *touched == 1 { "" } else { "s" }
        ),
        Target::Changeset {
            id,
            title,
            just_started: true,
            ..
        } => format!(
            "[{prefix}:{slug} · draft {} \"{title}\" · started]",
            short_id(*id)
        ),
        Target::Changeset {
            id, title, touched, ..
        } => format!(
            "[{prefix}:{slug} · draft {} \"{title}\" · {touched} file{}]",
            short_id(*id),
            if *touched == 1 { "" } else { "s" }
        ),
    }
}

fn io_err(msg: String) -> SpaceError {
    SpaceError::Io(msg)
}

fn map_changeset_err(e: ChangesetError) -> ProjectError {
    match e {
        ChangesetError::Foreign { id } => {
            SpaceError::ChangesetForeign { id: id.to_string() }.into()
        }
        other => ProjectError::Changeset(other),
    }
}

/// Enforces `cap` (when set) and decodes to UTF-8 — the single place
/// every read path applies the cap and conversion, so they can't drift
/// apart. `cap: None` lifts the check entirely.
pub fn text_from_bytes(bytes: Vec<u8>, cap: Option<usize>) -> Result<String, ProjectError> {
    if let Some(cap) = cap {
        if bytes.len() > cap {
            return Err(io_err(format!(
                "file too large ({} bytes > {cap} cap); use `grep` to extract a slice",
                bytes.len()
            ))
            .into());
        }
    }
    String::from_utf8(bytes).map_err(|e| io_err(format!("non-utf8: {e}")).into())
}

/// Translate a glob pattern (`*`, `**`, `?`) into a regex anchored at
/// both ends. Single `*` matches a single path segment (no `/`); `**`
/// matches zero or more segments.
fn glob_to_regex(pattern: &str) -> Result<regex::Regex, regex::Error> {
    let mut out = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    out.push_str(".*");
                    i += 2;
                    // `**/` collapses the trailing slash too so the
                    // pattern matches at any directory depth.
                    if i < chars.len() && chars[i] == '/' {
                        i += 1;
                    }
                } else {
                    out.push_str("[^/]*");
                    i += 1;
                }
            }
            '?' => {
                out.push_str("[^/]");
                i += 1;
            }
            '.' | '+' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out.push('$');
    regex::Regex::new(&out)
}

/// Slice `content` to the requested 1-based, inclusive line range.
/// `end == -1` (or unset) means EOF.
fn apply_view_range(content: &str, view_range: Option<(i64, i64)>) -> String {
    let Some((start, end)) = view_range else {
        return content.to_string();
    };
    let start = start.max(1) as usize;
    let lines: Vec<&str> = content.lines().collect();
    let end_idx = if end < 0 || end as usize > lines.len() {
        lines.len()
    } else {
        end as usize
    };
    if start > end_idx {
        return String::new();
    }
    lines[start - 1..end_idx].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_strips_prefix_and_slug() {
        let r = parse_space_path("space:oncall/runbooks/foo.md").unwrap();
        assert_eq!(r.slug, "oncall");
        assert_eq!(r.rel_path, "runbooks/foo.md");
    }

    #[test]
    fn parse_root_with_trailing_slash() {
        let r = parse_space_path("space:oncall/").unwrap();
        assert_eq!(r.slug, "oncall");
        assert_eq!(r.rel_path, "");
    }

    #[test]
    fn parse_root_without_slash() {
        let r = parse_space_path("space:oncall").unwrap();
        assert_eq!(r.slug, "oncall");
        assert_eq!(r.rel_path, "");
    }

    #[test]
    fn parse_returns_none_for_non_space_paths() {
        assert!(parse_space_path("/etc/passwd").is_none());
        assert!(parse_space_path("oncall/foo.md").is_none());
        assert!(parse_space_path("").is_none());
    }

    #[test]
    fn parse_rejects_empty_slug() {
        assert!(parse_space_path("space:").is_none());
        assert!(parse_space_path("space:/foo").is_none());
    }

    #[test]
    fn parse_with_changeset() {
        let id = ChangesetId::new();
        let with_rel = format!("space:oncall@{id}/runbooks/foo.md");
        let r = parse_space_path(&with_rel).unwrap();
        assert_eq!(r.slug, "oncall");
        assert_eq!(r.rel_path, "runbooks/foo.md");
        assert_eq!(r.changeset_id, Some(id));

        let root_only = format!("space:oncall@{id}");
        let root = parse_space_path(&root_only).unwrap();
        assert_eq!(root.slug, "oncall");
        assert_eq!(root.rel_path, "");
        assert_eq!(root.changeset_id, Some(id));
    }

    #[test]
    fn parse_without_changeset_leaves_it_none() {
        let r = parse_space_path("space:oncall/foo.md").unwrap();
        assert_eq!(r.changeset_id, None);
    }

    #[test]
    fn parse_rejects_bad_changeset_id() {
        assert!(parse_space_path("space:oncall@not-a-uuid/foo.md").is_none());
        assert!(parse_space_path("space:oncall@/foo.md").is_none());
    }

    #[test]
    fn target_main_has_no_draft_space_target() {
        assert_eq!(Target::Main.space_target(), drua_library::SpaceTarget::Main);
    }

    #[test]
    fn target_changeset_space_target_carries_its_handle() {
        let id = ChangesetId::new();
        let handle = drua_library::DraftHandle::new(
            drua_library::DraftName::from(uuid::Uuid::from(id)),
            "deadbeef",
        );
        let target = Target::Changeset {
            id,
            handle: handle.clone(),
            status: ChangesetStatus::Open,
            title: "a draft".to_string(),
            touched: 1,
            just_started: false,
        };
        assert_eq!(
            target.space_target(),
            drua_library::SpaceTarget::Draft(handle)
        );
    }

    #[test]
    fn map_changeset_err_translates_foreign_into_space_error() {
        let id = ChangesetId::new();
        let mapped = map_changeset_err(ChangesetError::Foreign { id });
        assert!(matches!(
            mapped,
            ProjectError::Space(SpaceError::ChangesetForeign { id: ref s }) if *s == id.to_string()
        ));
    }

    #[test]
    fn map_changeset_err_falls_back_to_project_changeset_variant() {
        let mapped = map_changeset_err(ChangesetError::UnsupportedActor);
        assert!(matches!(
            mapped,
            ProjectError::Changeset(ChangesetError::UnsupportedActor)
        ));
    }

    #[test]
    fn bare_space_path_matches_only_slugless_uri() {
        assert!(is_bare_space_path("space:"));
        assert!(is_bare_space_path("space:/"));
        assert!(is_bare_space_path("space://"));
        assert!(!is_bare_space_path("space:demo"));
        assert!(!is_bare_space_path("space:demo/"));
        assert!(!is_bare_space_path("space:/foo"));
        assert!(!is_bare_space_path(""));
        assert!(!is_bare_space_path("/etc/passwd"));
    }

    #[test]
    fn bad_request_error_stringifies_distinctly() {
        let err = SpaceError::BadRequest {
            reason: "'space:/foo' is not a valid space URI; expected 'space:<slug>' or 'space:<slug>/<rel>'".into(),
        };
        let s = err.to_string();
        assert!(
            s.contains("BadRequest"),
            "expected error to surface BadRequest token, got: {s}"
        );
        assert!(!s.contains("Unauthorized"), "got: {s}");
        assert!(!s.contains("NotFound"), "got: {s}");
    }

    #[test]
    fn not_found_and_bad_request_are_distinct_strings() {
        let nf = SpaceError::NotFound {
            slug: "ghost".into(),
        }
        .to_string();
        let br = SpaceError::BadRequest {
            reason: "empty slug".into(),
        }
        .to_string();
        assert_ne!(nf, br);
        assert!(nf.contains("NotFound"));
        assert!(br.contains("BadRequest"));
    }

    #[test]
    fn validate_rejects_traversal() {
        assert!(SpaceFs::validate_rel_path("../etc").is_err());
        assert!(SpaceFs::validate_rel_path("foo/../etc").is_err());
    }

    #[test]
    fn normalize_strips_dot_segments() {
        assert_eq!(normalize_rel_path("."), "");
        assert_eq!(normalize_rel_path("./"), "");
        assert_eq!(normalize_rel_path("./foo"), "foo");
        assert_eq!(normalize_rel_path("foo/."), "foo");
        assert_eq!(normalize_rel_path("foo/./bar"), "foo/bar");
        assert_eq!(normalize_rel_path("foo/bar"), "foo/bar");
        assert_eq!(normalize_rel_path(""), "");
    }

    #[test]
    fn normalize_then_validate_accepts_dot_paths() {
        for raw in [".", "./", "./foo", "foo/.", "foo/./bar"] {
            let normalized = normalize_rel_path(raw);
            assert!(
                SpaceFs::validate_rel_path(&normalized).is_ok(),
                "expected `{raw}` (normalised: `{normalized}`) to validate"
            );
        }
    }

    #[test]
    fn normalize_does_not_collapse_traversal() {
        assert_eq!(normalize_rel_path(".."), "..");
        assert_eq!(normalize_rel_path("foo/../bar"), "foo/../bar");
        assert!(SpaceFs::validate_rel_path(&normalize_rel_path("..")).is_err());
        assert!(SpaceFs::validate_rel_path(&normalize_rel_path("foo/../bar")).is_err());
    }

    #[test]
    fn validate_rejects_absolute() {
        assert!(SpaceFs::validate_rel_path("/etc/passwd").is_err());
        assert!(SpaceFs::validate_rel_path("\\windows").is_err());
    }

    #[test]
    fn validate_rejects_nul() {
        assert!(SpaceFs::validate_rel_path("foo\0bar").is_err());
    }

    #[test]
    fn validate_rejects_double_slash() {
        assert!(SpaceFs::validate_rel_path("foo//bar").is_err());
    }

    #[test]
    fn validate_accepts_normal_paths() {
        assert!(SpaceFs::validate_rel_path("README.md").is_ok());
        assert!(SpaceFs::validate_rel_path("runbooks/foo/bar.md").is_ok());
        assert!(SpaceFs::validate_rel_path("").is_ok());
    }

    #[test]
    fn view_range_inclusive() {
        let txt = "a\nb\nc\nd\ne";
        assert_eq!(apply_view_range(txt, Some((2, 4))), "b\nc\nd");
    }

    #[test]
    fn view_range_open_end() {
        let txt = "a\nb\nc";
        assert_eq!(apply_view_range(txt, Some((2, -1))), "b\nc");
    }

    #[test]
    fn view_range_clamps_overshoot() {
        let txt = "a\nb";
        assert_eq!(apply_view_range(txt, Some((1, 10))), "a\nb");
    }

    #[test]
    fn view_range_empty_when_inverted() {
        let txt = "a\nb\nc";
        assert_eq!(apply_view_range(txt, Some((3, 1))), "");
    }

    #[test]
    fn view_range_none_returns_full() {
        let txt = "a\nb\nc";
        assert_eq!(apply_view_range(txt, None), "a\nb\nc");
    }

    #[test]
    fn glob_star_matches_one_segment() {
        let r = glob_to_regex("*.md").unwrap();
        assert!(r.is_match("foo.md"));
        assert!(!r.is_match("foo/bar.md"));
    }

    #[test]
    fn glob_double_star_matches_any_depth() {
        let r = glob_to_regex("**/*.md").unwrap();
        assert!(r.is_match("foo.md"));
        assert!(r.is_match("foo/bar.md"));
        assert!(r.is_match("a/b/c/x.md"));
    }

    #[test]
    fn glob_question_matches_one_char() {
        let r = glob_to_regex("?.md").unwrap();
        assert!(r.is_match("a.md"));
        assert!(!r.is_match("ab.md"));
    }

    fn blobs(paths: &[&str]) -> Vec<(String, Vec<u8>)> {
        paths.iter().map(|p| (p.to_string(), Vec::new())).collect()
    }

    #[test]
    fn glob_blobs_matches_relative_to_root() {
        let b = blobs(&[
            "runs/r1/draft/run.json",
            "runs/r1/draft/a--b.json",
            "runs/r1/checks.json",
            "state.json",
        ]);
        assert_eq!(
            glob_blobs(b.clone(), "runs/r1", "draft/*--*.json").unwrap(),
            vec!["runs/r1/draft/a--b.json"]
        );
        assert_eq!(
            glob_blobs(b.clone(), "runs/r1/draft", "*.json").unwrap(),
            vec!["runs/r1/draft/a--b.json", "runs/r1/draft/run.json"]
        );
        assert_eq!(
            glob_blobs(b, "runs/r1", "*.json").unwrap(),
            vec!["runs/r1/checks.json"]
        );
    }

    #[test]
    fn glob_blobs_root_empty_is_unchanged() {
        let b = blobs(&["runs/r1/draft/a--b.json", "lib/x.js"]);
        assert_eq!(
            glob_blobs(b.clone(), "", "runs/r1/draft/*--*.json").unwrap(),
            vec!["runs/r1/draft/a--b.json"]
        );
        assert_eq!(
            glob_blobs(b, "", "*.js").unwrap(),
            Vec::<String>::new(),
            "`*` still does not cross `/`"
        );
    }

    #[test]
    fn glob_blobs_double_star_under_root() {
        let b = blobs(&["runs/r1/draft/a--b.json", "runs/r2/draft/c--d.json"]);
        assert_eq!(
            glob_blobs(b, "runs/r1", "**/*.json").unwrap(),
            vec!["runs/r1/draft/a--b.json"]
        );
    }

    #[test]
    fn glob_blobs_root_is_a_file_matches_its_basename() {
        let b = blobs(&["lib/runtime.js"]);
        assert_eq!(
            glob_blobs(b.clone(), "lib/runtime.js", "*.js").unwrap(),
            vec!["lib/runtime.js"]
        );
        assert_eq!(
            glob_blobs(b, "lib/runtime.js", "*.md").unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn grep_blobs_glob_filter_matches_relative_to_root() {
        let b = vec![
            ("docs/a.md".to_string(), b"hello\n".to_vec()),
            ("docs/a.txt".to_string(), b"hello\n".to_vec()),
        ];
        let args = sandbox::GrepInput {
            pattern: "hello".into(),
            glob: Some("*.md".into()),
            ..Default::default()
        };
        let out = grep_blobs(b, "docs", &args).unwrap();
        assert_eq!(out, "docs/a.md");
    }

    fn dated(secs: i64) -> PathDates {
        let at = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0).unwrap();
        PathDates {
            created: at,
            modified: at,
        }
    }

    #[test]
    fn join_dates_looks_up_files_by_mapped_key() {
        let mut map = PathDatesMap::new();
        map.insert("research/a.md".into(), dated(100));
        let entries = vec!["a.md".to_string()];
        let out = join_dates(entries, Some(&map), |name| format!("research/{name}"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].entry, "a.md");
        assert_eq!(out[0].dates, Some(dated(100)));
    }

    #[test]
    fn join_dates_directories_never_get_dates() {
        let mut map = PathDatesMap::new();
        // Even a same-named entry in the map must not leak onto a dir —
        // `key_for` is never invoked for a trailing-slash entry.
        map.insert("research/".into(), dated(100));
        let entries = vec!["research/".to_string()];
        let out = join_dates(entries, Some(&map), |name| name.to_string());
        assert_eq!(out[0].dates, None);
    }

    #[test]
    fn join_dates_missing_key_is_none() {
        let map = PathDatesMap::new();
        let entries = vec!["ghost.md".to_string()];
        let out = join_dates(entries, Some(&map), |name| name.to_string());
        assert_eq!(out[0].dates, None);
    }

    #[test]
    fn join_dates_no_map_is_none_for_every_entry() {
        let entries = vec!["a.md".to_string(), "dir/".to_string()];
        let out = join_dates(entries, None, |name| name.to_string());
        assert!(out.iter().all(|e| e.dates.is_none()));
    }

    #[test]
    fn join_dates_glob_result_is_its_own_key() {
        let mut map = PathDatesMap::new();
        map.insert("research/a.md".into(), dated(200));
        let entries = vec!["research/a.md".to_string()];
        let out = join_dates(entries, Some(&map), |entry| entry.to_string());
        assert_eq!(out[0].dates, Some(dated(200)));
    }

    #[test]
    fn text_from_bytes_preserves_crlf() {
        let out = text_from_bytes(b"a\r\nb".to_vec(), Some(MAX_VIEW_FILE_BYTES)).unwrap();
        assert_eq!(out, "a\r\nb");
    }

    #[test]
    fn text_from_bytes_preserves_missing_final_newline() {
        let out = text_from_bytes(b"a\nb".to_vec(), Some(MAX_VIEW_FILE_BYTES)).unwrap();
        assert_eq!(out, "a\nb");
        assert!(!out.ends_with('\n'));
    }

    #[test]
    fn text_from_bytes_rejects_over_cap() {
        let bytes = vec![b'a'; MAX_VIEW_FILE_BYTES + 1];
        let err = text_from_bytes(bytes, Some(MAX_VIEW_FILE_BYTES))
            .unwrap_err()
            .to_string();
        assert!(err.contains("too large"), "got: {err}");
    }

    #[test]
    fn text_from_bytes_no_cap_allows_oversized() {
        let bytes = vec![b'a'; MAX_VIEW_FILE_BYTES + 1];
        let out = text_from_bytes(bytes, None).unwrap();
        assert_eq!(out.len(), MAX_VIEW_FILE_BYTES + 1);
    }

    #[test]
    fn text_from_bytes_rejects_invalid_utf8() {
        let err = text_from_bytes(vec![0xff, 0xfe], Some(MAX_VIEW_FILE_BYTES))
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-utf8"), "got: {err}");
    }

    fn changeset_target(just_started: bool) -> Target {
        let id = ChangesetId::new();
        Target::Changeset {
            id,
            handle: drua_library::DraftHandle::new(
                drua_library::DraftName::from(uuid::Uuid::from(id)),
                "deadbeef",
            ),
            status: ChangesetStatus::Open,
            title: "workflow-fix-typos run ab12cd34".to_string(),
            touched: 2,
            just_started,
        }
    }

    #[test]
    fn stamp_run_draft_form() {
        let target = changeset_target(false);
        let s = stamp(SpaceScheme::Space, "docs", &target, None, true);
        assert!(s.contains("space:docs · run draft "), "got: {s}");
        assert!(s.contains("2 files"), "got: {s}");
    }

    #[test]
    fn stamp_run_draft_preserves_typed_prefix() {
        let target = changeset_target(false);
        let s = stamp(SpaceScheme::Draft, "docs", &target, None, true);
        assert!(s.starts_with("[draft:docs · run draft "), "got: {s}");
    }

    #[test]
    fn stamp_run_no_draft_is_plain_main() {
        let s = stamp(SpaceScheme::Space, "docs", &Target::Main, None, true);
        assert_eq!(s, "[space:docs · main]");
    }

    #[test]
    fn stamp_non_run_just_started_form_unchanged() {
        let target = changeset_target(true);
        let s = stamp(SpaceScheme::Draft, "docs", &target, None, false);
        assert!(s.contains("started"), "got: {s}");
        assert!(!s.contains("run draft"), "got: {s}");
    }
}
