## Summary

<!-- What changed and why. Link the issue if there is one. -->

## Checklist

- [ ] Every commit is signed off (`git commit -s`, DCO).
- [ ] Commits follow Conventional Commits.
- [ ] Tests added or updated for behaviour changes.
- [ ] `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass locally.
- [ ] `bash scripts/pii-lint.sh` passes (no personal data, paths, emails or tokens).
- [ ] New dependencies have a reason above and pass `cargo deny check`.

## Platforms tested

<!-- Mark what you ran. Process-management changes need Windows and a Unix system. -->

- [ ] Windows x64
- [ ] Windows ARM64
- [ ] Linux
- [ ] macOS
- [ ] Not applicable (docs or CI only)

Notes on how you tested:

## Personal data

- [ ] This PR contains no usernames, home directory paths, hostnames, email
      addresses, tokens or real configuration files.
