use crate::string_summarizer::{apply_runs, collect_runs, SegmentedText, StringSummarizer};

pub struct RsyncFileList;

fn is_rsync_path_line(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    if line.starts_with(' ') || line.starts_with('\t') {
        return false;
    }
    if line.contains(' ') {
        return false;
    }
    line.contains('/') || line.contains('.') || line == "./"
}

impl StringSummarizer for RsyncFileList {
    fn name(&self) -> &'static str {
        "rsync-files"
    }
    fn can_summarize(&self, ctx: &SegmentedText) -> bool {
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(is_rsync_path_line))
    }
    fn apply(&self, ctx: &mut SegmentedText) -> bool {
        let runs = collect_runs(ctx, is_rsync_path_line);
        apply_runs(ctx, runs, "rsync-files", |count, _| {
            format!("{count} paths\n")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string_summarizer::StringSummarizerChain;

    fn run(raw: &str) -> SegmentedText {
        let mut ctx = SegmentedText::from_initial(raw);
        let chain = StringSummarizerChain::new().register(RsyncFileList);
        chain.run(&mut ctx);
        ctx
    }

    #[test]
    fn rsync_path_region_collapses() {
        let raw = "sending incremental file list\n\
                   created directory repo\n\
                   ./\n\
                   .gitignore\n\
                   gcloud-creds.json\n\
                   .git/\n\
                   .git/FETCH_HEAD\n\
                   .git/HEAD\n\
                   .git/cepler_environment\n\
                   modules/services/galoy-deps/vendor/galoy-deps/chart/\n\
                   modules/services/galoy-deps/vendor/galoy-deps/chart/templates/\n\
                   modules/services/workers/main.tf\n\
                   modules/services/workers/workers-values.yml.tmpl\n\
                   vendir/\n\
                   \n\
                   sent 11,493,167 bytes  received 11,273 bytes  2,091,716.36 bytes/sec\n\
                   total size is 11,441,431  speedup is 0.99\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(log.contains("<rsync-files"));
        assert!(log.contains("paths"));
        assert!(log.contains("</rsync-files>"));
        assert!(log.contains("sending incremental file list"));
        assert!(log.contains("created directory repo"));
        assert!(log.contains("sent 11,493,167 bytes"));
        assert!(log.contains("total size is 11,441,431"));
        assert!(!log.contains(".git/FETCH_HEAD"));
        assert!(!log.contains("workers-values.yml.tmpl"));
    }

    #[test]
    fn plain_prose_passes_through() {
        let raw = "Hello world\n\
                   This is just some prose with several lines.\n\
                   Nothing here looks like a path list.\n\
                   Goodbye.\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(!log.contains("<rsync-files"));
        assert!(log.contains("Hello world"));
        assert!(log.contains("Goodbye."));
    }

    #[test]
    fn lone_path_not_collapsed() {
        let raw = "header\nfile.txt\ntail with space\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(!log.contains("<rsync-files"));
        assert!(log.contains("file.txt"));
    }

    #[test]
    fn terraform_refreshing_state_not_collapsed() {
        let raw = "module.a.kubernetes_secret.x: Refreshing state... [id=ns/x]\n\
                   module.b.kubernetes_secret.y: Refreshing state... [id=ns/y]\n\
                   module.c.kubernetes_secret.z: Refreshing state... [id=ns/z]\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(!log.contains("<rsync-files"));
        assert!(log.contains("Refreshing state"));
    }

    #[test]
    fn indented_path_lines_not_collapsed() {
        let raw =
            "header\n   indented/path1.tf\n   indented/path2.tf\n   indented/path3.tf\nfooter\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(!log.contains("<rsync-files"));
        assert!(log.contains("indented/path1.tf"));
    }
}
