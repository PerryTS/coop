-- Contact form submissions table.
-- Applied by coop-daemon at deploy time.
CREATE TABLE IF NOT EXISTS contact_submissions (
    id SERIAL PRIMARY KEY,
    email TEXT NOT NULL,
    message TEXT NOT NULL,
    created_at BIGINT NOT NULL DEFAULT (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT
);

CREATE INDEX IF NOT EXISTS idx_contact_submissions_email ON contact_submissions(email);
