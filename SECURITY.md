# Security Policy

## Reporting a vulnerability

If you discover a security vulnerability in this project, please
report it responsibly. **Do not open a public GitHub issue.**

Email: security@anthropic.com

Include as much of the following as you can: a description of the
vulnerability, steps to reproduce, potential impact, and a suggested
fix if you have one. A minimal reproducer — a `Cargo.lock`, a
`flake.nix`, or the failing `nix build` invocation — is especially
helpful.

## Response

We will acknowledge your report and work with you on a timeline for
a fix and disclosure.

## Scope

This policy applies to the code in this repository. Vulnerabilities
in third-party dependencies (Nix itself, cargo, rustc, transitive
crates) should be reported to the upstream maintainer, though we
appreciate a heads-up if you believe the vulnerability affects this
project.

In particular we are interested in:

- Bugs that allow execution of attacker-controlled code, sandbox
  escape from the Nix evaluator, or substitution of a different
  artifact than the lockfile pins.
- Bugs that exfiltrate credentials from `CARGO_HOME`, `~/.netrc`, or
  `.cargo/config.toml` when resolving an untrusted workspace.

## Supported versions

| Version | Supported |
|---------|-----------|
| latest  | Yes       |
| older   | No        |

## Disclosure

We follow coordinated disclosure. After a fix is available, we will
publish a security advisory on GitHub. We ask that reporters refrain
from public disclosure until a fix has been released or 90 days have
passed from the initial report, whichever comes first.

## Recognition

With your permission, we will acknowledge your contribution in the
security advisory.
