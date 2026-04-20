/// The action being attempted on an [`super::AuthResource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthVerb {
    Create,
    Read,
    Update,
    Delete,
    /// Generic "use" — e.g. executing a sandbox tool or invoking a skill.
    Use,
}
