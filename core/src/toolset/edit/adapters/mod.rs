mod anthropic;
mod fallback;
mod gemini;
mod openai;
mod openai_v4a;

pub use anthropic::{AnthropicAdapter, AnthropicTextEditorVersion};
pub use fallback::FallbackAdapter;
pub use gemini::GeminiAdapter;
pub use openai::OpenAIAdapter;

pub(crate) use anthropic::edit_input_schema;
