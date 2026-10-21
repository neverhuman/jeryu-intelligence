# Contributing

## Pull requests

Outside pull requests are welcome on GitHub. This repository is a read-only
mirror of the forge, where contributions are gated, reviewed, and merged.
Nothing is merged on GitHub.

The importer is not live yet: we read GitHub pull requests but do not merge
them yet. Once it is live, it will bring pull requests to the forge. After a
forge merge, the mirror will receive the change and the GitHub pull request
will be closed with a link to the forge merge.

## Tests and fixtures

Use invented data in tests and fixtures, such as `acme`, `globex`, and
`service.example`. Do not include real site hosts, organization names, logins,
or filesystem paths; site-specific values belong in configuration.

## Local gate

From the repository root, with the pinned Rust toolchain installed, run:

```sh
JERYU_SPLIT_FULL_CHECK=1 ./ops/ci/check.sh
```
