-- Inline curation policies: per-Remote gates (min-age + webhook).
-- Explicit allow/block lists continue to use the existing curation_rules table.

CREATE TABLE IF NOT EXISTS curation_policies (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    remote_repo_id     UUID NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    enabled            BOOLEAN NOT NULL DEFAULT true,

    -- Built-in min-age cooldown gate
    min_age_enabled    BOOLEAN NOT NULL DEFAULT false,
    min_age_days       INT,

    -- Generic webhook gate
    webhook_enabled    BOOLEAN NOT NULL DEFAULT false,
    webhook_url        TEXT,
    webhook_timeout_ms INT NOT NULL DEFAULT 3000,
    webhook_fail_mode  TEXT NOT NULL DEFAULT 'closed' CHECK (webhook_fail_mode IN ('open', 'closed')),

    -- Stance when no explicit rule and no gate decides
    default_action     TEXT NOT NULL DEFAULT 'allow' CHECK (default_action IN ('allow', 'block')),

    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- A gate that is enabled must carry its config
    CONSTRAINT min_age_config CHECK (NOT min_age_enabled OR min_age_days IS NOT NULL),
    CONSTRAINT webhook_config CHECK (NOT webhook_enabled OR webhook_url IS NOT NULL)
);

-- One policy per Remote repo.
CREATE UNIQUE INDEX idx_curation_policies_repo ON curation_policies(remote_repo_id);
