use crate::string_summarizer::{apply_runs, collect_runs, SegmentedText, StringSummarizer};

pub struct TerraformInstallRun;

const FINDING_PREFIX: &str = "- Finding latest version of ";
const INSTALLING_PREFIX: &str = "- Installing ";
const INSTALLED_PREFIX: &str = "- Installed ";

fn is_install_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with(FINDING_PREFIX)
        || t.starts_with(INSTALLING_PREFIX)
        || t.starts_with(INSTALLED_PREFIX)
}

impl StringSummarizer for TerraformInstallRun {
    fn name(&self) -> &'static str {
        "terraform-install"
    }
    fn can_summarize(&self, ctx: &SegmentedText) -> bool {
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(is_install_line))
    }
    fn apply(&self, ctx: &mut SegmentedText) -> bool {
        let runs = collect_runs(ctx, is_install_line);
        apply_runs(ctx, runs, "terraform-install", |count, _| {
            format!("{count} provider lines\n")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string_summarizer::StringSummarizerChain;

    fn run(raw: &str) -> SegmentedText {
        let mut ctx = SegmentedText::from_initial(raw);
        let chain = StringSummarizerChain::new().register(TerraformInstallRun);
        chain.run(&mut ctx);
        ctx
    }

    #[test]
    fn provider_install_block_collapses() {
        let raw = "\
Initializing the backend...
- Finding latest version of hashicorp/google...
- Finding latest version of hashicorp/google-beta...
- Finding latest version of hashicorp/kubernetes...
- Finding latest version of hashicorp/helm...
- Installing hashicorp/google-beta v7.30.0...
- Installed hashicorp/google-beta v7.30.0 (signed, key ID 0C0AF313E5FD9F80)
- Installing hashicorp/kubernetes v3.1.0...
- Installed hashicorp/kubernetes v3.1.0 (signed, key ID 0C0AF313E5FD9F80)
- Installing hashicorp/helm v3.1.1...
- Installed hashicorp/helm v3.1.1 (signed, key ID 0C0AF313E5FD9F80)
- Installing hashicorp/google v7.30.0...
- Installed hashicorp/google v7.30.0 (signed, key ID 0C0AF313E5FD9F80)
done.
";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(log.contains("<terraform-install"));
        assert!(log.contains("12 provider lines"));
        assert!(log.contains("Initializing the backend..."));
        assert!(log.contains("done."));
        assert!(!log.contains("hashicorp/google-beta v7.30.0"));
    }

    #[test]
    fn non_matching_block_passes_through() {
        let raw = "header\nplain text\nmore plain text\ntail\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(!log.contains("<terraform-install"));
        assert!(log.contains("plain text"));
        assert!(log.contains("more plain text"));
    }

    #[test]
    fn isolated_single_line_passes_through() {
        let raw = "header\n- Installing hashicorp/google v7.30.0...\ntail\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(!log.contains("<terraform-install"));
        assert!(log.contains("- Installing hashicorp/google v7.30.0..."));
    }
}
