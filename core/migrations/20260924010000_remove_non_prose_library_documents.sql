-- R3.2: stop treating non-prose space files as searchable. The importer
-- filter change alone does not remove rows already indexed before this
-- migration — this is a one-off cleanup of those (embeddings live in the
-- same row, so this removes them too).
DELETE FROM library_documents
 WHERE doc_type = 'space_file'
   AND (path ~* '\.(json|js|mjs|cjs|ts|py|sh|rs|sql|lock|toml|png|jpe?g|gif|svg|emf|zip|gz)$'
        OR name IN ('.gitignore', '.gitattributes', '.gitkeep'));
