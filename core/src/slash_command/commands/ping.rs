use super::super::error::SlashCommandError;
use super::super::traits::{SlashCommand, SlashCommandContext, SlashCommandOutput};

pub struct PingCommand;

impl SlashCommand for PingCommand {
    fn name(&self) -> &str {
        "ping"
    }

    fn description(&self) -> &str {
        "Responds with pong"
    }

    fn execute(
        &self,
        _ctx: &SlashCommandContext,
        _args: &str,
    ) -> Result<SlashCommandOutput, SlashCommandError> {
        Ok(SlashCommandOutput::Text("pong".to_string()))
    }
}
