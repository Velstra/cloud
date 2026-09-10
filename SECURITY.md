# Reporting a vulnerability

**Do not open an issue.** A public issue is a disclosure, and the first people
to read it are not the ones running the software.

Write to **security@velstra.io** with:

- what the flaw is, in a sentence;
- how to reach it — the request, the resource, the role somebody needs;
- what it gets an attacker: data they should not read, a write they should not
  make, a machine they should not be on;
- the version, from `velstra --version` or the package's `Version` field.

A proof of concept is welcome and is never required. If you have one, say so
and we will ask for it over an encrypted channel rather than by email.

## What happens next

| When | What |
|---|---|
| Within 3 working days | An acknowledgement from a person, not a robot. |
| Within 10 working days | Our assessment: whether we reproduce it, how severe we think it is, and why. |
| Within 90 days | A fix in a release, or a written reason it is taking longer. |

We will tell you the release that carries the fix before it ships, and we
credit reporters by name unless asked not to. If we disagree about severity we
will say so plainly and explain the reasoning; you are free to disclose on your
own timeline, and we would rather know that in advance than find out.

## What is in scope

This repository — the API, the controller, the node and pool agents, the
consoles and the CLI — and the Debian package it builds.

The dependency graph is checked on every change (`deny.toml`, the
`supply-chain` job in CI): a published advisory against anything in it fails
the build. Each CI run also produces a **CycloneDX SBOM** as an artefact,
which is what makes "are you affected by CVE-2026-XXXX" a question with an
answer — the answer is a search of the bill of materials for the build that
was actually shipped, not a reading of `Cargo.toml`.

## What is not a vulnerability

- A cell run without TLS between its components. That is a deployment choice
  the platform reports rather than hides — the node status carries
  `consoleTls`, and the console says so on the terminal page.
- A tenant reaching their own data through their own credentials.
- Anything that needs an operator's cell-wide token to begin with. An operator
  can already do everything; that is what the role is.
