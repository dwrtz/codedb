PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS objects (
    hash TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    payload_json TEXT NOT NULL,
    payload_size_bytes INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS object_edges (
    parent_hash TEXT NOT NULL,
    child_hash TEXT NOT NULL,
    edge_label TEXT NOT NULL,
    edge_position INTEGER,
    PRIMARY KEY (parent_hash, child_hash, edge_label, edge_position),
    FOREIGN KEY (parent_hash) REFERENCES objects(hash) ON DELETE CASCADE,
    FOREIGN KEY (child_hash) REFERENCES objects(hash) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS migrations (
    hash TEXT PRIMARY KEY,
    parent_history_hash TEXT,
    input_root_hash TEXT NOT NULL,
    output_root_hash TEXT NOT NULL,
    operation_kind TEXT NOT NULL,
    operation_json TEXT NOT NULL,
    preconditions_json TEXT NOT NULL,
    postconditions_json TEXT NOT NULL,
    agent_json TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS histories (
    history_hash TEXT PRIMARY KEY,
    parent_history_hash TEXT,
    migration_hash TEXT NOT NULL,
    output_root_hash TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (migration_hash) REFERENCES migrations(hash) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS branches (
    name TEXT PRIMARY KEY,
    root_hash TEXT NOT NULL,
    history_hash TEXT,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (root_hash) REFERENCES objects(hash)
);

CREATE TABLE IF NOT EXISTS root_symbols (
    root_hash TEXT NOT NULL,
    symbol_hash TEXT NOT NULL,
    definition_hash TEXT NOT NULL,
    signature_hash TEXT NOT NULL,
    PRIMARY KEY (root_hash, symbol_hash),
    FOREIGN KEY (root_hash) REFERENCES objects(hash) ON DELETE CASCADE,
    FOREIGN KEY (symbol_hash) REFERENCES objects(hash),
    FOREIGN KEY (definition_hash) REFERENCES objects(hash),
    FOREIGN KEY (signature_hash) REFERENCES objects(hash)
);

CREATE TABLE IF NOT EXISTS root_names (
    root_hash TEXT NOT NULL,
    module_name TEXT NOT NULL,
    display_name TEXT NOT NULL,
    symbol_hash TEXT NOT NULL,
    is_preferred INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (root_hash, module_name, display_name),
    FOREIGN KEY (root_hash) REFERENCES objects(hash) ON DELETE CASCADE,
    FOREIGN KEY (symbol_hash) REFERENCES objects(hash)
);

CREATE TABLE IF NOT EXISTS dependencies (
    root_hash TEXT NOT NULL,
    from_symbol_hash TEXT NOT NULL,
    to_symbol_hash TEXT NOT NULL,
    PRIMARY KEY (root_hash, from_symbol_hash, to_symbol_hash),
    FOREIGN KEY (root_hash) REFERENCES objects(hash) ON DELETE CASCADE,
    FOREIGN KEY (from_symbol_hash) REFERENCES objects(hash),
    FOREIGN KEY (to_symbol_hash) REFERENCES objects(hash)
);

CREATE TABLE IF NOT EXISTS compile_cache (
    cache_key TEXT PRIMARY KEY,
    input_hash TEXT NOT NULL,
    backend TEXT NOT NULL,
    target TEXT NOT NULL,
    compiler_version TEXT NOT NULL,
    artifact_kind TEXT NOT NULL,
    artifact_hash TEXT NOT NULL,
    artifact_json TEXT,
    artifact_bytes BLOB,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (input_hash) REFERENCES objects(hash)
);

CREATE VIRTUAL TABLE IF NOT EXISTS source_search
USING fts5(root_hash, symbol_hash, rendered_source);
