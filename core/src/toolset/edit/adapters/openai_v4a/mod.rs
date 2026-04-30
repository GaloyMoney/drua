//! OpenAI V4A `apply_patch` envelope parser.
//!
//! Wire grammar (from the canonical
//! [`apply_patch_tool_instructions.md`](https://github.com/openai/codex/blob/main/codex-rs/apply-patch/apply_patch_tool_instructions.md)):
//!
//! ```text
//! *** Begin Patch
//! *** Add File: <path>
//! +<content lines>...
//! *** Update File: <path>
//! *** Move to: <new-path>          (optional, after Update File)
//! @@ <hunk header>                 (optional disambiguator)
//!  <unchanged context>
//! -<removed line>
//! +<added line>
//! *** End of File                  (optional, marks EOF inside hunk)
//! *** Delete File: <path>
//! *** End Patch
//! ```

mod parser;

#[cfg(test)]
mod tests;

pub use parser::parse_v4a_patch;
