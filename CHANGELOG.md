# Changelog

## [Unreleased]

### Fixed

- A session file whose name carries no timestamp (one kept under its own name, such as `current.jsonl`, after `omp --resume <path>`) is now reachable: its date directory, its date URL and its sid-prefix URL resolve it, taking the id and creation date from its header. Such a file was listed in `sessions.jsonl` but `session://<sid>` reported it missing. Stamped names keep their id and date from the name, and a stamped file still wins a shared id.

## [0.1.21] - 2026-11-15

### Security

- `session_query` `export` now refuses a target that reaches the omp sessions directory under another spelling (different letter case, a Windows 8.3 short name) or through a symbolic link; such a target used to be written into the sessions directory

## [0.1.20] - 2026-11-12

### Changed

- omp-sessions publishes from a daily release instead of a patch version on every push to `main`: installs come from the `release` branch (`https://github.com/oh-my-omp/omp-sessions#release`), and the package declares `omomp.changelog` and ships `CHANGELOG.md`, so the merged What's New shows what changed here.
