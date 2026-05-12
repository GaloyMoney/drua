//! Line-pattern passes: each compacts runs of "boring" lines (build
//! progress, dependency resolves, cache hits, …) into XML marker tags.
//! Order in `default_chain` matters — more specific patterns before
//! more generic ones.

mod cargo;
mod git;
mod nix;
mod rsync;
mod terraform_install;
mod terraform_state;

use crate::string_summarizer::StringSummarizerChain;

/// Default chain ordering, ported from the disconnected tool-classifier.
pub fn default_chain() -> StringSummarizerChain {
    StringSummarizerChain::new()
        .register(nix::NixDerivationPreprocessor)
        .register(nix::NixDrvList)
        .register(nix::NixFetchList)
        .register(nix::NixCopyRun)
        .register(nix::NixBuildingRun)
        .register(nix::NixCacheActivity)
        .register(nix::NixImageLayerRun)
        .register(cargo::CargoDownloadRun)
        .register(cargo::CargoCompileRun)
        .register(cargo::CargoCheckRun)
        .register(terraform_install::TerraformInstallRun)
        .register(terraform_state::TerraformRefreshRun)
        .register(terraform_state::TerraformDataSourceRun)
        .register(rsync::RsyncFileList)
        .register(git::GitCloneProgress)
}
