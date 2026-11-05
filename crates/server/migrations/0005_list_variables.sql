-- The pipeline list can show the variables a run was started with (Jenkins style) instead
-- of the commit title. Off by default: everyone who can read the repository sees the
-- values. Run variables stored before this have no `secret` flag; the list never shows
-- their values.
ALTER TABLE repos ADD COLUMN list_shows_variables BOOLEAN NOT NULL DEFAULT FALSE;
