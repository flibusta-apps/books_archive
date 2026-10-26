# books_archive

HTTP book source over a local directory of Flibusta-style dump zip archives
(`f.fb2-000024-030559.zip`, `fb2-123456-234567.zip`, ... — the name must end with
`-{start}-{end}.zip`, an inclusive id range). Entries are named `{id}.{ext}`.
Intended as a mirror source for `books_downloader`.

## Endpoints

- `GET /health` — liveness, always open.
- `GET /b/{id}/fb2` — fb2 book as `application/zip` with a single `{id}.fb2` entry (raw-copied, not recompressed). 404 if the book is not fb2.
- `GET /b/{id}/epub`, `GET /b/{id}/mobi` — raw file, only if the stored original has that extension, otherwise 404.
- `GET /b/{id}/download` — fb2 → zip as above; any other format → raw file with Content-Type by extension (`application/octet-stream` fallback).

Responses carry `Content-Length` and `Content-Disposition: attachment; filename="{id}.{ext}"` (`{id}.fb2.zip` for fb2). Errors: 400 bad id, 401 auth, 404 not found.

## Environment

- `ARCHIVE_DIR` — required; directory with the archives. Scanned recursively at startup only in range-scan mode (restart to rescan).
- `INPX_PATH` — optional; path to the `.inpx` catalog. If unset, a single `*.inpx` is looked up in `ARCHIVE_DIR` (non-recursive, then one level deep, e.g. `ARCHIVE_DIR/fb2.Flibusta.Net/*.inpx`).
- `API_KEY` — optional; if set, `/b/*` requires header `Authorization: <key>`.

## Lookup modes

**INPX mode** (default when an `.inpx` is found or `INPX_PATH` is set). The `.inpx` is parsed once at startup into an in-memory `LIBID -> (archive, entry name, ext)` map (titles/authors are not kept). Each `X.inp` inside it describes the archive `X.zip` located next to the `.inpx`. The zip entry is `{FILE}.{EXT}` (the id requested is the LIBID, so FILE may differ from it). The extension from the catalog decides `fb2`/`epub`/`mobi` handling without opening any archive, so a format mismatch is a 404 with no disk access. If the exact entry is not found, a basename-stem search `{id}.*` in that archive is tried. Missing archive file (e.g. still downloading) or missing entry → 404 with a warning log. Duplicate LIBIDs: a non-deleted row (DEL=0) beats a deleted one; otherwise the last row wins. Malformed lines are skipped and counted in one warning. Restart to reload the catalog.

**Range-scan mode** (fallback when no `.inpx` is found, or it fails to load). Archives must be named `...-{start}-{end}.zip`; candidate archives for an id are opened and searched for an entry whose basename stem equals the id.
