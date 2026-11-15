# Changelog

## [Unreleased]

### Fixed

- Session files whose names are not `<timestamp>_<id>.jsonl`, such as a session resumed by path and kept as `current.jsonl`, are now reachable through `session://`: they appear in their project's date listing at the creation date in their header, open through `session://projects/<bucket>/<date>/<sid>` and through a unique `session://<id-prefix>` (and so through `session_query`), and a file whose header gives no usable id or date stays unlisted. Before, these sessions were listed in `sessions.jsonl` but every `session://` route to them reported no session. When a timestamped file and a custom-named copy share a session id, `session://<sid>` still opens the timestamped file; the copy opens through its own date.

## [0.1.21] - 2026-11-15

### Security

- `session_query` `export` now refuses a target that reaches the omp sessions directory under another spelling (different letter case, a Windows 8.3 short name) or through a symbolic link; such a target used to be written into the sessions directory

## [0.1.20] - 2026-11-12

### Changed

- omp-sessions publishes from a daily release instead of a patch version on every push to `main`: installs come from the `release` branch (`https://github.com/oh-my-omp/omp-sessions#release`), and the package declares `omomp.changelog` and ships `CHANGELOG.md`, so the merged What's New shows what changed here.
