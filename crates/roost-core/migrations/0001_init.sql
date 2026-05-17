-- 0001_init: project + tab tables.
--
-- The data model: a Project is a sidebar entry with a name and a default
-- working directory. A Tab belongs to a Project and runs one shell at a
-- specific cwd. The `command` column is reserved for "task tabs" (saved
-- agent commands) and is always NULL in the MVP — UI doesn't write it.

CREATE TABLE project (
    id          INTEGER PRIMARY KEY,
    name        TEXT    NOT NULL,
    cwd         TEXT    NOT NULL,
    position    INTEGER NOT NULL,
    created_at  INTEGER NOT NULL
);

CREATE TABLE tab (
    id            INTEGER PRIMARY KEY,
    project_id    INTEGER NOT NULL REFERENCES project(id) ON DELETE CASCADE,
    title         TEXT,
    cwd           TEXT    NOT NULL,
    last_command  TEXT,
    command       TEXT,
    position      INTEGER NOT NULL,
    created_at    INTEGER NOT NULL,
    last_active   INTEGER NOT NULL
);

CREATE INDEX tab_project ON tab(project_id, position);
