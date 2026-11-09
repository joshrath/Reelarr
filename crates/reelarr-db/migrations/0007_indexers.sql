-- 0005 is reserved for Downloads' durable NZB admission. Forward-only, clean-room.
CREATE TABLE indexers (
    name TEXT PRIMARY KEY NOT NULL,
    settings_json TEXT NOT NULL CHECK (json_valid(settings_json) AND json_type(settings_json) = 'object'),
    extra_download_hosts TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(extra_download_hosts) AND json_type(extra_download_hosts) = 'array'
               AND json_array_length(extra_download_hosts) <= 16),
    api_key_ciphertext BLOB NOT NULL
);
