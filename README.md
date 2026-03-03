# vegas

What happens in Vegas, stays in Vegas — unless you decide to bring it home.

A filesystem sandboxing tool for Linux, written in Rust.  Run programs in a
temporary overlay of the real filesystem — see every change they make, then
choose to apply or throw away those changes.

## What it does

Vegas uses two core Linux kernel features:

* **Linux namespaces** (`CLONE_NEWNS`) – the sandboxed process gets its own
  private mount namespace so any mounts it creates never affect the host.
* **OverlayFS** – a union filesystem that layers a writable _upper_ directory
  on top of the real root filesystem as the read-only _lower_ layer.  All
  writes made by the sandboxed process land in the upper directory; the real
  filesystem is untouched.

The sandboxed program shares the live `/proc`, `/dev`, and `/sys` with the
host, so it sees the same running processes, devices, and kernel state as
everything else on the system.  Only _filesystem writes_ are isolated.

When the program exits vegas shows you exactly what changed, then asks:

```
What would you like to do with these changes?
  [a] Apply  – copy changes to the real filesystem
  [k] Keep   – save the sandbox for later inspection
  [d] Discard – throw away all changes (default)
```

## Requirements

* Linux kernel ≥ 4.0 with `CONFIG_OVERLAY_FS` enabled.
* Root privileges (run via `sudo`).

## Installation

```bash
cargo install --path .
```

## Usage

```bash
# Run a shell inside the sandbox (changes are isolated)
sudo vegas run -- bash

# Safely test a package installation
sudo vegas run -- apt install curl

# Run a script that modifies the system — review before applying
sudo vegas run -- ./my-setup-script.sh

# Run as a specific uid:gid inside the sandbox
sudo vegas run --user 1000:1000 -- my-script.sh
```

### `--user`

By default the sandboxed command runs as **root** (uid 0) so it can freely
modify system paths.  Those writes only go to the overlay upper directory; the
real filesystem is never touched.  Use `--user uid` or `--user uid:gid` to
run as a different user:

```bash
sudo vegas run --user 1000 -- my-unprivileged-script.sh
```

## How it works internally

```
┌─────────────────────────────────────────┐
│  Real filesystem  /  (read-only lower)  │
└────────────────────┬────────────────────┘
                     │ OverlayFS
┌────────────────────┼────────────────────┐
│  Upper directory   │  (captures writes) │
└────────────────────┼────────────────────┘
                     │
              ┌──────┴──────┐
              │  Sandbox    │  (CLONE_NEWNS)
              │  process    │
              └─────────────┘
```

1. Vegas creates `upper/`, `work/`, and `merged/` inside a temporary directory.
2. It mounts an OverlayFS: `lowerdir=/,upperdir=upper,workdir=work` at `merged/`.
3. `/proc`, `/dev`, and `/sys` are bind-mounted into `merged/` for live access.
4. A child process enters a new mount namespace, `chroot(2)`s into `merged/`,
   and `exec(2)`s the requested command.
5. When the command exits, Vegas walks the `upper/` directory to collect changes:
   - Regular files → added or modified
   - Character devices with rdev 0,0 → OverlayFS whiteouts (deletions)
6. The user chooses to apply, keep, or discard those changes.
