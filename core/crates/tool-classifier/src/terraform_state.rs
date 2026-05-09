use crate::string_summarizer::{apply_runs, collect_runs, SegmentedText, StringSummarizer};

pub struct TerraformRefreshRun;
pub struct TerraformDataSourceRun;

fn is_refresh_line(line: &str) -> bool {
    let t = line.trim_start();
    t.contains(": Refreshing state... [id=")
}

fn is_data_source_line(line: &str) -> bool {
    let t = line.trim_start();
    if !t.starts_with("data.") {
        return false;
    }
    t.contains(": Reading...") || t.contains(": Read complete after ")
}

impl StringSummarizer for TerraformRefreshRun {
    fn name(&self) -> &'static str {
        "terraform-refresh"
    }
    fn can_summarize(&self, ctx: &SegmentedText) -> bool {
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(is_refresh_line))
    }
    fn apply(&self, ctx: &mut SegmentedText) -> bool {
        let runs = collect_runs(ctx, is_refresh_line);
        apply_runs(ctx, runs, "terraform-refresh", |count, _| {
            format!("{count} resources refreshed\n")
        })
    }
}

impl StringSummarizer for TerraformDataSourceRun {
    fn name(&self) -> &'static str {
        "terraform-data-source"
    }
    fn can_summarize(&self, ctx: &SegmentedText) -> bool {
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(is_data_source_line))
    }
    fn apply(&self, ctx: &mut SegmentedText) -> bool {
        let runs = collect_runs(ctx, is_data_source_line);
        apply_runs(ctx, runs, "terraform-data-source", |count, _| {
            format!("{count} data-source lines\n")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string_summarizer::StringSummarizerChain;

    fn run(raw: &str) -> SegmentedText {
        let mut ctx = SegmentedText::from_initial(raw);
        let chain = StringSummarizerChain::new()
            .register(TerraformRefreshRun)
            .register(TerraformDataSourceRun);
        chain.run(&mut ctx);
        ctx
    }

    #[test]
    fn refresh_run_collapses_contiguous_lines() {
        let raw = "header\n\
                   module.workers.kubernetes_namespace_v1.workers: Refreshing state... [id=galoy-agents-concourse]\n\
                   module.workers.kubernetes_secret_v1.concourse_worker: Refreshing state... [id=galoy-agents-concourse/galoy-agents-concourse-worker]\n\
                   module.workers.helm_release.workers: Refreshing state... [id=galoy-agents-concourse]\n\
                   tail\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(log.contains("<terraform-refresh"));
        assert!(log.contains("3 resources refreshed"));
        assert!(log.contains("</terraform-refresh>"));
        assert!(log.contains("header"));
        assert!(log.contains("tail"));
        assert!(!log.contains("Refreshing state..."));
    }

    #[test]
    fn refresh_non_matching_passes_through() {
        let raw = "Plan: 3 to add, 0 to change, 0 to destroy.\n\
                   Apply complete! Resources: 3 added.\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(!log.contains("<terraform-refresh"));
        assert!(log.contains("Plan: 3 to add"));
        assert!(log.contains("Apply complete!"));
    }

    #[test]
    fn lone_refresh_line_left_alone() {
        let raw = "header\n\
                   module.workers.helm_release.workers: Refreshing state... [id=foo]\n\
                   tail\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(!log.contains("<terraform-refresh"));
        assert!(log.contains("Refreshing state..."));
    }

    #[test]
    fn data_source_run_collapses_contiguous_lines() {
        let raw = "header\n\
                   data.google_client_config.default: Reading...\n\
                   data.google_container_cluster.primary: Reading...\n\
                   data.google_client_config.default: Read complete after 0s [id=projects//regions//zones/]\n\
                   data.google_container_cluster.primary: Read complete after 0s [id=projects/galoy-agents/locations/.../clusters/galoy-agents-cluster]\n\
                   tail\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(log.contains("<terraform-data-source"));
        assert!(log.contains("4 data-source lines"));
        assert!(log.contains("</terraform-data-source>"));
        assert!(log.contains("header"));
        assert!(log.contains("tail"));
        assert!(!log.contains("data.google_client_config"));
    }

    #[test]
    fn data_source_non_matching_passes_through() {
        let raw = "Initializing the backend...\n\
                   Initializing provider plugins...\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(!log.contains("<terraform-data-source"));
        assert!(log.contains("Initializing the backend"));
    }

    #[test]
    fn lone_data_source_line_left_alone() {
        let raw = "header\n\
                   data.google_client_config.default: Reading...\n\
                   tail\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(!log.contains("<terraform-data-source"));
        assert!(log.contains("data.google_client_config.default: Reading..."));
    }
}
