pub struct LibraryConfig {
    pub data_dir: String,
    pub repo_url: String,
    /// How often the fetcher task pulls from origin.
    pub fetch_interval_ms: u64,
}
