-- NNTP provider settings (#76 PR 3). Forward-only, clean-room.
-- Exactly one provider row (id = 1): Reelarr has no multi-server fallback yet.
-- The password is XChaCha20-Poly1305 ciphertext from SecretCipher (NULL when
-- the provider needs no login); plaintext never reaches this table.
CREATE TABLE nntp_server (
    id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    settings_json TEXT NOT NULL CHECK (json_valid(settings_json) AND json_type(settings_json) = 'object'),
    password_ciphertext BLOB CHECK (password_ciphertext IS NULL OR length(password_ciphertext) > 0)
);
