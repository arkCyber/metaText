<!--
Thanks for contributing to metaText! Please read CONTRIBUTING.md and fill in
the sections below. Keep the pull request focused on a single change.
-->

## Summary

<!-- What does this change do, and why? -->

## Related issues

<!-- e.g. "Closes #12", "Relates to #34" -->

## Type of change

- [ ] Bug fix (non-breaking change that fixes an issue)
- [ ] New feature (non-breaking change that adds functionality)
- [ ] Breaking change (fix or feature that changes existing behaviour)
- [ ] Documentation only
- [ ] Refactor / internal cleanup
- [ ] Build / CI

## How has this been tested?

<!-- Describe the tests you added or ran, with the exact commands. -->

```
cargo fmt --all -- --check
cargo clippy --all-targets --all-features
cargo test --all-features
```

## Checklist

- [ ] My code follows the project's coding standards
      (English, `cargo fmt`, no new clippy warnings).
- [ ] I have documented every new public item (`#![deny(missing_docs)]`).
- [ ] I have added tests that prove my fix/feature works.
- [ ] I have updated the documentation (`README.md` and/or rustdoc).
- [ ] I have updated `CHANGELOG.md` for user-visible changes.
- [ ] My changes generate no new warnings and pass CI.
