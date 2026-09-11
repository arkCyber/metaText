# Security Policy

## Supported versions

metaText is in active development. Security fixes are applied to the latest
release on the `main` branch.

| Version | Supported          |
| ------- | ------------------ |
| 0.4.x   | :white_check_mark: |
| < 0.4   | :x:                |

## Reporting a vulnerability

**Please do not report security vulnerabilities through public GitHub issues.**

Instead, use GitHub's
[private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
("Report a vulnerability" under the **Security** tab), or email
[arksong2018@gmail.com](mailto:arksong2018@gmail.com).

Please include:

- The affected version(s) and platform.
- A description of the issue and its impact.
- Reproduction steps or a proof of concept, if possible.
- Any suggested mitigation.

You can expect an acknowledgement within **72 hours** and a more detailed
response within **7 days**. We will keep you informed of the progress toward a
fix and credit you in the release notes unless you prefer to remain anonymous.

## Scope and threat model

metaText is an early-stage, decentralized messaging client. When assessing a
report, keep the following in mind:

- Transport is plain TCP; payloads are sealed with **ChaCha20-Poly1305** and the
  shared key is derived from `--passphrase` with **Argon2**.
- Without `--passphrase`, each process uses a fresh random key and messages stay
  local.
- The Tox DHT bootstrap list in `config.toml` is **not** dialled yet; peers are
  reached through `--peer` / `/connect`.
- Contact identifiers are opaque DID-like strings with no per-contact key
  exchange yet.

In-scope examples: key derivation weaknesses, nonce reuse, frame parsing that
leads to panics or unbounded allocation, authentication bypass.

Out-of-scope examples: denial of service against your own loopback instance,
weaknesses inherent to sharing a passphrase, and the documented
[Known limitations](README.md#known-limitations).

## Recommended deployment practices

- Always set a strong, unique `--passphrase`; never reuse it across networks.
- Run behind a firewall and expose the listener only to trusted peers.
- Treat the session file (`metatext-session.json`) and SQLite database as
  sensitive local state.
