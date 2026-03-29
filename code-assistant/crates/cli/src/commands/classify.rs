use std::io::{self, Read};
use std::path::PathBuf;

use code_assistant_core::classifier::Classifier;

use crate::config::Config;

/// Run ONNX-based classification on code from stdin or a file.
pub fn run(config: &Config, text: Option<String>, model_dir: Option<String>) -> anyhow::Result<()> {
    let dir = model_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| config.model_dir());

    println!("Loading classifier from {}...", dir.display());
    let mut classifier = Classifier::load(&dir)?;
    println!(
        "Loaded: {} classes — {:?}\n",
        classifier.labels().len(),
        classifier.labels()
    );

    let input = match text {
        Some(t) => t,
        None => {
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf)?;
            buf
        }
    };

    if input.trim().is_empty() {
        anyhow::bail!("No input text provided. Pass --text or pipe to stdin.");
    }

    let prediction = classifier.classify(&input)?;

    println!("Predicted: {}", prediction.label);
    println!("Confidence: {:.1}%\n", prediction.confidence * 100.0);

    println!("All scores:");
    for (label, score) in &prediction.scores {
        let bar_len = (score * 40.0) as usize;
        let bar: String = "█".repeat(bar_len);
        println!("  {:<25} {:>6.2}%  {}", label, score * 100.0, bar);
    }

    Ok(())
}
