-- `root_path` records where the *unwrapped* upstream value lives within the
-- transport-wrapped `structured_content` envelope. JSON-pointer-ish syntax:
--
--   $          → upstream returned a record (envelope is a no-op)
--   $.items    → upstream returned a top-level array
--   $.value    → upstream returned a string / number / bool / null /
--                non-JSON text (reify wrapped it)
--
-- Used at fetch / render / compose time to recover the upstream's actual
-- shape from the wrapped form.
--
-- DESTRUCTIVE: pre-existing rows hold `original_structured` / `raw_text` in
-- the post-#316 wrapped shape and `summary._recover` templates that point
-- into wrapped paths (`$.items`, `$.value`). The new code expects unwrapped
-- storage and unwrapped paths — no conversion is performed. Wipe the table
-- so every row in the cache matches the new contract; tool invocations are
-- transient (TTL'd) so this is acceptable. Going forward the column is
-- populated per-row by `persist_classification`.

TRUNCATE TABLE public.tool_invocations;

ALTER TABLE public.tool_invocations
    ADD COLUMN root_path text NOT NULL DEFAULT '$';
