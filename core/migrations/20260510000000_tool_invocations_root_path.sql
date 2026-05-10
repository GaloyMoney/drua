-- `root_path` records where the *unwrapped* upstream value lives within the
-- transport-wrapped `structured_content` envelope. JSON-pointer-ish syntax:
--
--   $          → upstream returned a record (envelope is a no-op)
--   $.items    → upstream returned a top-level array
--   $.value    → upstream returned a string / number / bool / null /
--                non-JSON text (reify wrapped it)
--
-- Used at fetch / render / compose time to recover the upstream's actual
-- shape from the wrapped form. Existing rows default to `$` — they were
-- persisted before this column existed and their wrapping (if any) is
-- recoverable by sniffing the envelope keys.

ALTER TABLE public.tool_invocations
    ADD COLUMN root_path text NOT NULL DEFAULT '$';
