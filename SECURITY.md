# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities through [GitHub private vulnerability reporting](https://github.com/civitaspo/herdr-linear-agent/security/advisories/new) for this repository.

Do not open a public issue for security-sensitive reports.

## Credentials

This repository must never hold strong credentials such as GPG private keys, machine-user personal access tokens, or server application private keys. Those secrets belong only in `civitaspo/securefix-server`.

The plugin itself must never store Linear tokens in the repository, in configuration files, or in environment variables. Tokens belong in the macOS Keychain.
