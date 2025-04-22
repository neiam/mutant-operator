-- init/init.sql
CREATE TABLE jobs
(
    id        SERIAL PRIMARY KEY,
    label     TEXT    NOT NULL,
    env       JSONB   NOT NULL,
    instances INTEGER NOT NULL
);

CREATE TABLE job_history
(
    job_id INTEGER PRIMARY KEY
);

-- Example job (spawns 3 pods with test env vars)
INSERT INTO jobs (label, env, instances)
VALUES ('pinenote',
        '[
          {
            "name": "SNAP_RES",
            "value": "960x540"
          }
        ]',
        3);
