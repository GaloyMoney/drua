//! `Grep` — content search across files. Two backends:
//!
//! - `space:<slug>/...` paths route through `SpaceFs::grep`.
//! - Anything else forwards to the sandbox-server's `Grep` handler
//!   via `/execute`. Both shell out to `rg`.
//!
//! Read-only: executable with either `SandboxUse` or `SandboxRead`.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::GrepInput;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::sandbox::Sandboxes;
use crate::space_fs::SpaceFs;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for, OutputSchema, TextOutput};

pub struct Grep {
    sandboxes: Arc<Sandboxes>,
    space_fs: Arc<SpaceFs>,
}

impl Grep {
    pub fn new(sandboxes: Arc<Sandboxes>, space_fs: Arc<SpaceFs>) -> Self {
        Self {
            sandboxes,
            space_fs,
        }
    }
}

static GREP_OUTPUT: LazyLock<OutputSchema<TextOutput>> = LazyLock::new(OutputSchema::new);
static GREP_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<GrepInput>);

#[async_trait::async_trait]
impl TopLevelTool for Grep {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search file contents using ripgrep. Accepts either an in-sandbox path \
         or a `space:<slug>/...` path that reads from the project's mounted spaces."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &GREP_SCHEMA
    }

    fn inner_output_schema(&self) -> Option<&serde_json::Value> {
        Some(GREP_OUTPUT.schema())
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        // See bash.rs.
        subject.can_use_agent_file_tools()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let mut input: GrepInput = parse_params(arguments)?;
        Audit::record_action("grep");

        if let Some(t) = input.type_.as_ref() {
            if t.trim().is_empty() {
                input.type_ = None;
            } else if !is_known_rg_type(t) {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Grep failed: unknown ripgrep `type` {t:?}. \
                     `type` is a ripgrep language tag (e.g. `rust`, `py`, `ts`, `js`, \
                     `md`, `json`, `toml`, `yaml`, `sh`, `go`, `c`, `cpp`, `java`). \
                     For filename patterns use `glob` (e.g. `\"glob\": \"**/*.rs\"`).",
                ))]));
            }
        }

        let path_for_space = input.path.as_deref().unwrap_or("");
        let space_output = self.space_fs.grep(subject, path_for_space, &input).await?;

        if let Some(output) = space_output {
            let out = TextOutput {
                output: output.clone(),
            };
            return Ok(GREP_OUTPUT.success(output, &out));
        }

        let sandbox_id = subject
            .readable_sandbox_id()
            .ok_or_else(|| super::sandbox_read_denied("Grep"))?;
        Audit::record_sandbox_id(sandbox_id);

        let client = self
            .sandboxes
            .instance_client_for_read(subject, sandbox_id)
            .await?;

        match client.execute_grep(&input).await {
            Ok(resp) => {
                let out = TextOutput {
                    output: resp.output.clone(),
                };
                Ok(if resp.is_error {
                    GREP_OUTPUT.error(resp.output, &out)
                } else {
                    GREP_OUTPUT.success(resp.output, &out)
                })
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "sandbox /execute call failed: {e}"
            ))])),
        }
    }
}

fn is_known_rg_type(t: &str) -> bool {
    matches!(
        t,
        "ada"
            | "agda"
            | "asciidoc"
            | "asm"
            | "asp"
            | "ats"
            | "avro"
            | "awk"
            | "bat"
            | "bazel"
            | "bitbake"
            | "bzip2"
            | "c"
            | "cabal"
            | "candid"
            | "cbor"
            | "ceylon"
            | "clojure"
            | "cmake"
            | "cmd"
            | "coffeescript"
            | "config"
            | "coq"
            | "cpp"
            | "creole"
            | "crystal"
            | "cs"
            | "csharp"
            | "cshtml"
            | "css"
            | "csv"
            | "cuda"
            | "cython"
            | "d"
            | "dart"
            | "devicetree"
            | "dhall"
            | "diff"
            | "docker"
            | "dockercompose"
            | "dts"
            | "dvc"
            | "ebuild"
            | "edn"
            | "elisp"
            | "elixir"
            | "elm"
            | "erb"
            | "erlang"
            | "fennel"
            | "fidl"
            | "fish"
            | "flatbuffers"
            | "fortran"
            | "fsharp"
            | "fut"
            | "gap"
            | "gn"
            | "go"
            | "gradle"
            | "graphql"
            | "groovy"
            | "gzip"
            | "h"
            | "haml"
            | "hare"
            | "haskell"
            | "hbs"
            | "hs"
            | "html"
            | "hy"
            | "idris"
            | "janet"
            | "java"
            | "jinja"
            | "jl"
            | "js"
            | "json"
            | "jsonl"
            | "julia"
            | "jupyter"
            | "k"
            | "kotlin"
            | "lean"
            | "less"
            | "license"
            | "lilypond"
            | "lisp"
            | "log"
            | "lua"
            | "lz4"
            | "lzma"
            | "m4"
            | "make"
            | "mako"
            | "man"
            | "markdown"
            | "matlab"
            | "md"
            | "meson"
            | "mint"
            | "mk"
            | "ml"
            | "motoko"
            | "msbuild"
            | "nim"
            | "nix"
            | "objc"
            | "objcpp"
            | "ocaml"
            | "octave"
            | "odin"
            | "org"
            | "pants"
            | "pascal"
            | "pdf"
            | "perl"
            | "php"
            | "pkgconfig"
            | "po"
            | "pod"
            | "postscript"
            | "prolog"
            | "protobuf"
            | "ps"
            | "pug"
            | "puppet"
            | "purs"
            | "py"
            | "qmake"
            | "qml"
            | "r"
            | "racket"
            | "raku"
            | "rdoc"
            | "readme"
            | "reasonml"
            | "red"
            | "rescript"
            | "robot"
            | "rst"
            | "ruby"
            | "rust"
            | "sass"
            | "scala"
            | "scheme"
            | "sh"
            | "slim"
            | "slm"
            | "smarty"
            | "sml"
            | "snakemake"
            | "solidity"
            | "soy"
            | "spark"
            | "spec"
            | "sql"
            | "stylus"
            | "sv"
            | "svg"
            | "swift"
            | "swig"
            | "systemd"
            | "taskpaper"
            | "tcl"
            | "terraform"
            | "tex"
            | "texinfo"
            | "textile"
            | "tf"
            | "thrift"
            | "toml"
            | "tres"
            | "ts"
            | "twig"
            | "typoscript"
            | "vala"
            | "vb"
            | "vcl"
            | "verilog"
            | "vhdl"
            | "vim"
            | "vimscript"
            | "webidl"
            | "wgsl"
            | "wiki"
            | "xml"
            | "xz"
            | "yacc"
            | "yaml"
            | "yang"
            | "z"
            | "zig"
            | "zsh"
            | "zstd"
    )
}
