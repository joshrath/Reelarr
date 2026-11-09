# reelarr-stream

Playback for Reelarr. Direct play is implemented; FFmpeg remuxing and HLS
transcoding (FFmpeg always run as a separate process, redistributable builds
only) come later.

## Endpoints

Both need a signed-in session (the `reelarr_session` cookie). A same-origin
`<video>` element sends it automatically.

### `GET /api/v1/media/{id}/playback`

Says how the requesting browser (from `User-Agent`) should play the file, as
`reelarr_types::PlaybackInfo`:

```json
{
  "media_file_id": 1,
  "method": "direct_play",
  "stream_url": "/api/v1/media/1/stream",
  "mime_type": "video/mp4",
  "unsupported": [],
  "duration_ms": 7260000
}
```

When `method` is `"unsupported"`, `stream_url` and `mime_type` are null and
`unsupported` lists every reason (`kind`: `container`, `video_codec`,
`audio_codec` or `not_probed`, plus the ffprobe `name` and a `message` to
show the user).

### `GET` / `HEAD /api/v1/media/{id}/stream`

The file's bytes with `Accept-Ranges: bytes`, `ETag` and `Last-Modified`.
Single byte ranges get `206` with `Content-Range`; ranges past the end get
`416` with `Content-Range: bytes */<len>`; malformed or multi-range headers
get the full `200` response. `If-Range` and `If-None-Match` are honored.
Unknown ids, missing files, and anything that resolves (via symlinks or
`..`) outside its library root are `404`.

## Direct-play rules

| Container | Video | Audio |
|---|---|---|
| MP4 / MOV | H.264 (8-bit) everywhere; HEVC on Safari only; AV1 and VP9 on Chromium and Firefox | AAC and MP3 everywhere; FLAC on Safari, Chromium and Firefox; Opus on Chromium and Firefox; AC-3 and E-AC-3 on Safari only |
| WebM | VP8, VP9 and AV1 on Chromium and Firefox | Opus and Vorbis on Chromium and Firefox |

Anything else, including MKV, AVI, MPEG-TS, 10-bit H.264 and DTS/TrueHD, is
reported as unsupported until remuxing and transcoding land. All iOS and
iPadOS browsers are treated as Safari because they use WebKit. Unknown
clients only get H.264 with AAC or MP3 in MP4.

## Wiring

The server holds an `Arc<dyn MediaLookup>` in `AppState::media`. The default
is an empty in-memory lookup; the database-backed lookup comes with the
reelarr-db stores.
