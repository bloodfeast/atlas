# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's private vulnerability reporting: open the
repository's **Security** tab and choose **Report a vulnerability**
(https://github.com/bloodfeast/atlas/security/advisories/new). Do not open a public issue for a
vulnerability.

Include what you found, the version (`atlas --version`), your platform, and the steps or input that show
it. You will get an answer in the advisory thread; a fix ships as a new signed release, and the advisory is
published with it.

## Supported versions

Only the latest release gets fixes. atlas updates itself at session start unless updates are turned off,
so the fix reaches installs that keep updates on.

## Scope

atlas runs on your machine and makes no network calls at query time. Its threat model, path confinement and
the deny-list of files it never reads are described on the wiki's
[Security model](https://github.com/bloodfeast/atlas/wiki/Security-model) page. Reports about any of these
are in scope, as are the release signing and the update check.
