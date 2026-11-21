# Security policy

cpu-control is normally run as root, so mistakes in it matter more than its size suggests.

## Supported versions

There are no releases yet. Only the latest commit on `main` is supported; fixes are not backported.

## Reporting a vulnerability

Report it privately through GitHub: the **Report a vulnerability** button under the repository's
[Security tab](https://github.com/schiz0x00/cpu-control/security/advisories/new). Please do not open a
public issue.

Useful to include: the commit you tested, the exact command or profile file, what happened, and what you
expected.

This is a personal project maintained in spare time. Reports are handled on a best-effort basis and there
is no guaranteed response time and no bug bounty.

## What counts

In scope, for example:

- a profile file, command-line argument, environment variable or cache file that makes the tool, when run
  through `sudo` or the GUI's `pkexec` prompt, write or change ownership of something other than the
  intended CPU sysfs files, its own cache, or the profiles file it was pointed at;
- a way for another local user to influence what a root invocation does;
- a way to get past the checks described under "Is it safe to run?" in the README.

Not a vulnerability in this project:

- anything that requires giving the tool passwordless root (a `NOPASSWD` sudoers entry or a polkit rule
  that allows it without a password). `--profiles` and `--sysfs-root` choose the files it reads and
  writes, so that configuration is equivalent to handing out root and is explicitly unsupported;
- a user who is already root, or who knowingly authenticates, making the machine slow or unresponsive by
  taking CPUs offline. That is what the tool is for;
- running a binary from a directory an attacker can already write to.

## Known weak points

Stated here so nobody has to rediscover them:

- nothing has been tested as root by the automated tests, including the code that hands files back to the
  invoking user under `sudo`;
- the GUI's `pkexec` path is not covered by tests;
- the dependency tree is large (about 550 crates in `Cargo.lock`, mostly from the GUI toolkit) and has not
  been audited.
