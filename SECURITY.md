# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately. Do not open a public issue.

- Email **jag.reehal+security@gmail.com**, or
- use GitHub's private vulnerability reporting: [Security → Report a vulnerability](https://github.com/jagreehal/openapi-axum/security/advisories/new).

Include the crate version, a minimal reproducer, and the impact you expect. You should get a reply within 7 days.

## Scope

Request validation runs before your handler, against the published document. It doesn't replace authorization: your handlers and extractors still decide who may do what. `verify_responses` buffers each response in memory to check it, so leave `VerifyMode::Panic` out of production.

## Supported versions

Fixes go into the latest published version. Older versions do not get backports.
