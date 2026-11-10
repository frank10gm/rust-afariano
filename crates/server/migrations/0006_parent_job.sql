-- A pipeline started through the API with a job's CI_JOB_TOKEN remembers that job, so
-- the pages say "Started by release:gate in #9" instead of showing two unrelated
-- pipelines of the same commit.
ALTER TABLE pipelines ADD COLUMN parent_job_id BIGINT REFERENCES jobs(id) ON DELETE SET NULL;
CREATE INDEX pipelines_parent_job ON pipelines (parent_job_id) WHERE parent_job_id IS NOT NULL;
