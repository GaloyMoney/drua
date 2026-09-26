/// The action being attempted on an [`super::AuthResource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthVerb {
    Create,
    Read,
    Update,
    Delete,
    /// Generic "use" — e.g. executing a sandbox tool or invoking a skill.
    Use,
    /// Write only inside a changeset (`AuthResource::Space`): open,
    /// bind, and edit through the git-branch staging area, but never
    /// `main` directly. See `handoff-space-changesets-2026-09-23.md`.
    Propose,
}
