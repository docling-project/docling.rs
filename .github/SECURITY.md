# Security policy

docling.rs converts untrusted documents, so a crafted input that crashes the
process, exhausts memory or CPU out of proportion to its size, reaches the
network or reads local files it should not is a vulnerability.
[`docs/SECURITY.md`](../docs/SECURITY.md) has the threat model, the limits
that are on by default, and which denial-of-service reports are in scope and
how they are scored.

## Supported versions

The latest release. Fixes are not backported.

## Reporting a vulnerability

Please **do not** report a security problem in a public issue, discussion or
pull request. Use GitHub's private vulnerability reporting:

1. Open the [Security tab](https://github.com/docling-project/docling.rs/security)
   of this repository.
2. Click **Report a vulnerability**.
3. Describe the problem: the input or steps that trigger it, the version, and
   the surface (library, CLI, `docling-serve`, MCP server, Python / Node /
   WebAssembly binding).

Keep the details private until a fixed release is out.
