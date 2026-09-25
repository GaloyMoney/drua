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
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
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

/// Which of the two path schemes (D9) a [`SpaceRef`] was parsed from.
/// `space:` resolves by authority (§3 rule 3); `draft:` always targets
/// the caller's own draft, for anyone who holds `Propose`.
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

/// Parsed view of a `space:<slug>`, `space:<slug>/<rel>`,
/// `space:<slug>@<changeset-id>/<rel>`, or `draft:<slug>/<rel>` path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SpaceRef<'a> {
    scheme: SpaceScheme,
    slug: &'a str,
    /// Empty for the space root (`space:<slug>` or `space:<slug>/`).
    rel_path: &'a str,
    /// Explicit `@<changeset-id>` override (§6.4) — `space:` only;
    /// `draft:` never carries one (D9). Slugs can't contain `@`
    /// (`validate_slug`), so splitting on the first one is unambiguous.
    changeset_id: Option<ChangesetId>,
}

/// Returns `Some(SpaceRef)` iff `path` starts with the `space:` or
/// `draft:` prefix, has a non-empty slug, and — when an `@<id>` suffix
/// is present — that id parses as a `ChangesetId`. Anything else
/// returns `None` so callers can fall through to the existing sandbox
/// dispatch (or, for a `space:`/`draft:`-prefixed path that just fails
/// to parse, a `BadRequest` raised by the caller).
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

/// True for slugless space URIs (`space:`, `space:/`, `draft:`, etc.);
/// routed to `list_mounted_spaces` for runtime discovery. `draft:`
/// alone is accepted for symmetry, even though it has nothing extra to
/// list beyond what `space:` already shows.
fn is_bare_space_path(path: &str) -> bool {
    let rest = path
        .strip_prefix("space:")
        .or_else(|| path.strip_prefix("draft:"));
    let Some(rest) = rest else {
        return false;
    };
    rest.trim_matches('/').is_empty()
}

/// What a `space:` call resolves to (§2.1) — `main` directly, or the tip
/// of an in-flight changeset branch. Reads and writes route through
/// `Spaces`'s `at`/`target_ref` parameters accordingly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Main,
    Changeset {
        id: ChangesetId,
        /// Current branch tip, from `Changesets::ensure_ref` — may be
        /// ahead of the entity's last-observed `head_oid` (e.g. a human
        /// push landed on the branch since).
        tip: String,
        status: ChangesetStatus,
        /// `title`, `touched`, and `just_started` are carried along
        /// purely for the D10/D19 stamp — never used for git
        /// addressing.
        title: String,
        touched: usize,
        /// rev3 §5.3: true only for the `draft:` write that just
        /// lazily created the draft — renders "started" instead of the
        /// touched-file count, so the caller's very first write gets
        /// explicit first-write feedback.
        just_started: bool,
    },
}

impl Target {
    fn git_ref(&self) -> Option<String> {
        match self {
            Target::Main => None,
            Target::Changeset { id, .. } => Some(Changeset::git_ref_for(*id)),
        }
    }

    fn at(&self) -> Option<&str> {
        match self {
            Target::Main => None,
            Target::Changeset { tip, .. } => Some(tip.as_str()),
        }
    }
}

/// Whether a `resolve` call is a read or a write — a write additionally
/// requires a `Target::Changeset` to be `Open` (§2.2: "only `Open`
/// accepts writes"). Read/write *authorization* (`Propose` vs `Update`)
/// lands in PR 4 of this handoff's sequencing; unrelated to this check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    Read,
    Write,
}

/// Auth-gated, resolved view of a `space:<slug>/<rel>` or
/// `draft:<slug>/<rel>` path.
struct Resolved {
    space: Space,
    /// Owned so the bundle outlives the input `&str`.
    rel_path: String,
    target: Target,
    /// D10: the stamp line for this resolution, computed once here so
    /// every caller (14 different file ops) gets it for free instead
    /// of re-deriving it from `target`.
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
                resolved.target.at(),
            )
            .await?
            .ok_or_else(|| SpaceError::PathNotFound {
                slug: resolved.space.slug.clone(),
                path: resolved.rel_path,
            })?;
        Ok(Some(bytes))
    }

    /// D10: the stamp line for `path`, resolved exactly as the paired
    /// read/write call would (`write` must match — a stamp fetched with
    /// the wrong intent could show `main` for what's about to become a
    /// lazily-created draft, or vice versa). Safe to call before or
    /// after the paired op: `resolve` is idempotent per actor
    /// (`Changesets::draft_for` returns the existing draft on a second
    /// call), so this never creates a second draft or disagrees with
    /// what the paired call resolved to — it costs one extra mount +
    /// draft lookup, not a second write. `Ok(None)` for a non-
    /// `space:`/`draft:` path, so callers can no-op the model-facing
    /// prefix without a second branch.
    pub async fn resolved_stamp(
        &self,
        sub: &AuthSubject,
        path: &str,
        write: bool,
    ) -> Result<Option<String>, ProjectError> {
        let intent = if write { Intent::Write } else { Intent::Read };
        Ok(self.resolve(sub, path, intent).await?.map(|r| r.stamp))
    }

    /// Pure peek — does `path` start with the `space:` or `draft:`
    /// prefix and have a non-empty slug? Useful for short-circuiting
    /// tool dispatch before any auth or IO.
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

    /// `Ok(None)` for non-`space:`/`draft:` paths (caller falls through
    /// to sandbox). A `space:`/`draft:`-prefixed path that doesn't parse
    /// returns `BadRequest`, so malformed input never masquerades as an
    /// auth denial. Error precedence: bad URI → not found → not mounted
    /// → `Unauthorized` — the mount gate (`space_for_subject`) always
    /// runs before target resolution's write gate.
    async fn resolve(
        &self,
        sub: &AuthSubject,
        path: &str,
        intent: Intent,
    ) -> Result<Option<Resolved>, ProjectError> {
        let Some(sref) = parse_space_path(path) else {
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
        let space = self.projects.space_for_subject(sub, sref.slug).await?;
        let rel_path = normalize_rel_path(sref.rel_path);
        Self::validate_rel_path(&rel_path)?;
        let target = self.resolve_target(sub, &space, &sref, intent).await?;
        let differs = self
            .differs_note(sub, &sref, &rel_path, &target, intent)
            .await;
        let stamp = stamp(sref.scheme, &space.slug, &target, differs.as_deref());
        Ok(Some(Resolved {
            space,
            rel_path,
            target,
            stamp,
        }))
    }

    /// §3's resolution rule, rev3-amended:
    ///
    /// 1. `space:<slug>@<id>` — the explicit target (§6.4). Writes
    ///    require `Propose` (checked before the `Open` check — an
    ///    unauthorized subject shouldn't learn a changeset's status)
    ///    and an `Open` changeset.
    /// 2. `draft:<slug>` — always `sub`'s own draft: `Propose` required;
    ///    a write lazily creates it (`Changesets::draft_for`); a read
    ///    with none open overlays nothing and falls back to `Main`
    ///    (there's nothing to differ from yet — §5.3's "no draft" form).
    /// 3. `space:<slug>`: a **read** always sees `Main` (D19's differs
    ///    stamp is computed separately, in `differs_note`, never by
    ///    overlaying). A **write** requires `Update`; a subject with
    ///    only `Propose` gets `UseDraft` (rev3 D9/D15 — replaces rev2's
    ///    silent redirect into a draft); neither verb is `Unauthorized`.
    ///    A subject that holds `Update` but already has an open draft
    ///    gets `DraftOpen` (D15) rather than silently landing on `main`.
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
                sub.can(AuthVerb::Propose, AuthResource::Space(Some(space.id)))?;
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
                sub.can(AuthVerb::Propose, AuthResource::Space(Some(space.id)))?;
                if intent != Intent::Write {
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
                let had_draft = self
                    .changesets
                    .open_draft_for(sub)
                    .await
                    .map_err(map_changeset_err)?
                    .is_some();
                let cs = self
                    .changesets
                    .draft_for(sub, None, None, Some(sref.rel_path))
                    .await
                    .map_err(map_changeset_err)?;
                self.changeset_target(cs, !had_draft).await
            }
            SpaceScheme::Space => {
                if intent != Intent::Write {
                    return Ok(Target::Main);
                }
                if sub
                    .can(AuthVerb::Update, AuthResource::Space(Some(space.id)))
                    .is_ok()
                {
                    return match self
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
                    };
                }
                // No `Update` — a `Propose`-only subject must stage
                // through `draft:` instead; neither verb is a plain
                // `Unauthorized`.
                sub.can(AuthVerb::Propose, AuthResource::Space(Some(space.id)))?;
                Err(SpaceError::UseDraft {
                    slug: space.slug.clone(),
                }
                .into())
            }
        }
    }

    /// `touched` (D10's `<n> files`) is computed here, once, right
    /// where the hydrated entity is already in hand — never re-fetched
    /// downstream just to render a stamp. Best-effort: a `touched_count`
    /// failure degrades to `0` rather than failing the whole op.
    /// `just_started` (D19/§5.3) is set only by the `draft:` write that
    /// lazily created the draft.
    async fn changeset_target(
        &self,
        cs: Changeset,
        just_started: bool,
    ) -> Result<Target, ProjectError> {
        let touched = self.changesets.touched_count(&cs).await.unwrap_or(0);
        let tip = self
            .changesets
            .ensure_ref(&cs)
            .await
            .map_err(map_changeset_err)?;
        Ok(Target::Changeset {
            id: cs.id,
            tip,
            status: cs.status,
            title: cs.title,
            touched,
            just_started,
        })
    }

    /// D19: for a `space:` **read** that resolved to `Main`, the short
    /// id of the caller's open draft iff that draft has touched
    /// `rel_path` in this same space — `None` in every other case
    /// (`draft:` reads, writes, no open draft, or an untouched path).
    /// Best-effort: any lookup failure degrades to `None` rather than
    /// failing the read over a stamp.
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
        let at = resolved.target.at();

        // Try as a directory first; if it's a tree, list it. If not a
        // tree, fall through to a blob read.
        if let Some(entries) = self
            .spaces
            .list_dir(&resolved.space.slug, &resolved.rel_path, at)
            .await
            .map_err(|e| -> ProjectError { e.into() })?
        {
            return Ok(Some(FileView::Dir(format_dir(entries))));
        }

        let bytes = self
            .spaces
            .read_file(&resolved.space.slug, &resolved.rel_path, at)
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
                resolved.target.at(),
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
                resolved.target.at(),
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

    /// Blind overwrite of `space:<slug>/<rel>` with `content`.
    #[instrument(name = "library.space_fs.write_file", skip(self, sub, content))]
    pub async fn write_file(
        &self,
        sub: &AuthSubject,
        path: &str,
        content: String,
    ) -> Result<Option<()>, ProjectError> {
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
                resolved.target.git_ref().as_deref(),
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        self.record_write(&resolved.target, oid, "write_file", &resolved.rel_path)
            .await?;
        Ok(Some(()))
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
    ) -> Result<Option<()>, ProjectError> {
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
                resolved.target.git_ref().as_deref(),
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        self.record_write(&resolved.target, oid, "str_replace", &resolved.rel_path)
            .await?;
        Ok(Some(()))
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
    ) -> Result<Option<()>, ProjectError> {
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
                resolved.target.git_ref().as_deref(),
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        self.record_write(&resolved.target, oid, "insert", &resolved.rel_path)
            .await?;
        Ok(Some(()))
    }

    /// Removes the file at `space:<slug>/<rel>`. Success even if the
    /// file was already gone.
    #[instrument(name = "library.space_fs.delete_file", skip(self, sub))]
    pub async fn delete_file(
        &self,
        sub: &AuthSubject,
        path: &str,
    ) -> Result<Option<()>, ProjectError> {
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
                resolved.target.git_ref().as_deref(),
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        self.record_write(&resolved.target, oid, "delete_file", &resolved.rel_path)
            .await?;
        Ok(Some(()))
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
    ) -> Result<Option<()>, ProjectError> {
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
        // An explicit `@<changeset-id>` on one side and not the other,
        // or a `space:`/`draft:` scheme mismatch, is ambiguous — which
        // target does the move belong to? — rather than silently
        // picking one (§5.1: the two schemes can resolve to different
        // targets for the same slug).
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
                from_resolved.target.git_ref().as_deref(),
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        self.record_write(&from_resolved.target, oid, "move_file", &to_rel)
            .await?;
        Ok(Some(()))
    }

    /// Glob walk across the space's tree. Pattern is the standard
    /// glob syntax (`*`, `**`, `?`); matches against the relative
    /// path inside `spaces/<slug>/`. `path`'s rel-component anchors
    /// the search root — a directory, or a single file; naming neither
    /// is an error.
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
        Ok(Some(glob_blobs(blobs, pattern)?))
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
        let files = glob_blobs(blobs, pattern)?;
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
        Ok(Some(grep_blobs(blobs, args)?))
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
                resolved.target.at(),
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

    /// Stamps `Audit::record_changeset_id` before `commit_attribution`
    /// runs, when the target is a changeset — extends the commit's
    /// trailer block with `Drua-Changeset` (`user/mod.rs`'s trailer
    /// loop) for provenance through a squash-merge (§7, §10). Must run
    /// before `commit_attribution`, not after — that's why this isn't
    /// folded into `record_write`, which only sees the *result* of the
    /// write.
    fn record_changeset_audit(target: &Target) {
        if let Target::Changeset { id, .. } = target {
            Audit::record_changeset_id(*id);
        }
    }

    /// After a write lands on a `Target::Changeset`, records it on the
    /// entity (`Changesets::record_commit`) so `status`'s commit count
    /// and history stay accurate. No-op for `Target::Main` or a
    /// no-op write (`oid: None` — the tree was unchanged).
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

/// Filter `blobs` (rel-path, bytes) by a glob pattern and return the
/// matching paths, sorted.
fn glob_blobs(blobs: Vec<(String, Vec<u8>)>, pattern: &str) -> Result<Vec<String>, SpaceError> {
    let regex = glob_to_regex(pattern)
        .map_err(|e| io_err(format!("invalid glob pattern '{pattern}': {e}")))?;
    let mut out: Vec<String> = blobs
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| regex.is_match(p))
        .collect();
    out.sort();
    Ok(out)
}

/// Run `grep` over already-walked blobs. Mirrors the curated subset of
/// flags the `Grep` top-level tool accepts.
fn grep_blobs(
    blobs: Vec<(String, Vec<u8>)>,
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
            if !g.is_match(&rel) {
                continue;
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

/// First 8 characters of a `ChangesetId`'s string form — D10's stamp
/// format everywhere it names an id.
fn short_id(id: ChangesetId) -> String {
    id.to_string().chars().take(8).collect()
}

/// D10/D19/§5.3: the stamp line prepended to every `space:`/`draft:`
/// file-tool result. `prefix` is whichever scheme the caller actually
/// typed (`space:` or `draft:`); the two can resolve to the same
/// `Target::Changeset`, and the stamp should say what was asked for,
/// not just what it means. `differs` (only ever `Some` for a
/// `space:` read resolved to `Main`) names the caller's own draft when
/// it has touched the same path — D19's "differs in your draft" form.
fn stamp(scheme: SpaceScheme, slug: &str, target: &Target, differs: Option<&str>) -> String {
    let prefix = scheme.prefix();
    match target {
        // rev3 D9: `draft:` never falls back silently to `main` — a
        // read with no open draft says so explicitly.
        Target::Main if scheme == SpaceScheme::Draft => format!("[draft:{slug} · no draft]"),
        Target::Main => match differs {
            Some(id) => format!("[space:{slug} · main · differs in your draft {id}]"),
            None => format!("[space:{slug} · main]"),
        },
        // Reached only via the explicit `space:<slug>@<id>` form —
        // `draft:` paths never point at a non-`Open` changeset.
        Target::Changeset {
            id, status, title, ..
        } if scheme == SpaceScheme::Space && *status != ChangesetStatus::Open => {
            format!(
                "[space:{slug}@{} · changeset \"{title}\" · {status:?}]",
                short_id(*id)
            )
        }
        // rev3 §5.3: the write that lazily created the draft gets
        // "started" instead of the touched-file count.
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

/// Remaps a `Changesets` service failure reached through target
/// resolution into the model-facing `SpaceError` family where one
/// exists (`Foreign`), and into `ProjectError::Changeset` otherwise —
/// `UnsupportedActor`/`MainUnborn`/etc. aren't expected on this path
/// (every subject reaching `SpaceFs` is a real actor and `main` is
/// never unborn once a space exists), but a type-safe fallback beats a
/// panic if one somehow surfaces.
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
    fn target_main_has_no_git_ref_or_at() {
        assert_eq!(Target::Main.git_ref(), None);
        assert_eq!(Target::Main.at(), None);
    }

    #[test]
    fn target_changeset_names_its_branch_ref_and_tip() {
        let id = ChangesetId::new();
        let target = Target::Changeset {
            id,
            tip: "deadbeef".to_string(),
            status: ChangesetStatus::Open,
            title: "a draft".to_string(),
            touched: 1,
            just_started: false,
        };
        assert_eq!(target.git_ref(), Some(format!("refs/heads/drua/{id}")));
        assert_eq!(target.at(), Some("deadbeef"));
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
}
