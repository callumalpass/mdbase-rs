CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY,
    mtime_ns INTEGER NOT NULL,
    ctime_ns INTEGER,
    size INTEGER NOT NULL,
    frontmatter_json TEXT NOT NULL,
    body TEXT NOT NULL,
    effective_json TEXT,
    parse_error INTEGER DEFAULT 0
);

CREATE TABLE IF NOT EXISTS file_types (
    path TEXT NOT NULL,
    type_name TEXT NOT NULL,
    PRIMARY KEY (path, type_name)
);

CREATE TABLE IF NOT EXISTS links (
    source_path TEXT NOT NULL,
    target_path TEXT NOT NULL,
    source_revision TEXT NOT NULL DEFAULT '',
    resolved INTEGER NOT NULL DEFAULT 0,
    location TEXT NOT NULL,
    field TEXT,
    raw_target TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS unique_values (
    type_name TEXT NOT NULL,
    field_name TEXT NOT NULL,
    value TEXT NOT NULL,
    path TEXT NOT NULL,
    PRIMARY KEY (type_name, field_name, value)
);

CREATE TABLE IF NOT EXISTS identity_values (
    value TEXT PRIMARY KEY,
    path TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_file_types_type ON file_types(type_name);
CREATE INDEX IF NOT EXISTS idx_links_target ON links(target_path);
CREATE INDEX IF NOT EXISTS idx_links_source ON links(source_path);
CREATE INDEX IF NOT EXISTS idx_identity_values_path ON identity_values(path);
