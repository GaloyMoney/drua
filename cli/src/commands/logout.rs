use anyhow::Result;

use crate::config::Config;

pub fn run() -> Result<()> {
    Config::delete()?;
    println!("Logged out");
    Ok(())
}
