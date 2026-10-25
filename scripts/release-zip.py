#!/usr/bin/env python3
"""Build the archive that gets delivered: sources, the prebuilt static binary, the built dashboard.

    python3 scripts/release-zip.py

One command, no arguments, and it refuses rather than shipping something surprising. It is
the counterpart to `scripts/build-static.sh` and `dashboard/`'s build: run those first, then
this. The archive is a *product*, not a backup, so the file list is decided by rules:

* excluded — `node_modules`, `target`, `.venv`, `__pycache__`, `.cache`, `dist-server`,
  `.git`, and anything under `data/` (that is somebody's running database, not source);
* refused outright — a dot-env or key material anywhere in the tree is a build error, because
  the one thing a self-hosted product must never leak is its own `.env`;
* required — `main.py`, `bin/aerona-<platform>`, `dashboard/dist/index.html`, the app shell
  `dashboard/dist/shell.html` and its `theme-boot.js`, at least one `dashboard/dist/assets/*.css`,
  the launcher tests. A release missing the built dashboard is a
  source dump, and the whole point of this archive is that the user needs no build tooling; the
  stylesheet check is what proves the panel was compiled rather than copied.
* budgeted — 128 MB and 10 000 entries, checked for the repo *and* the zip, so a stray
  `target/` or an accidental video in `public/` fails the run instead of failing the upload.

Writes `aerona-v<version>-<platform>.zip` plus a `.md5` sibling into the directory above the
repo, and prints the probes a reviewer needs: the binary's own md5 and `--version`, the entry
count, and the size.
"""
from __future__ import annotations

import hashlib
import os
import platform
import subprocess
import sys
import time
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PARENT = ROOT.parent
MAX_BYTES = 128 * 1024 * 1024
MAX_ENTRIES = 10_000

EXCLUDE_DIRS = {
    "node_modules", "target", ".cache", "dist-server", "__pycache__", ".git", ".venv",
    "data", "tmp", ".pytest_cache", ".ruff_cache", "coverage", "build",
}
# Never in the archive, whatever directory they hide in.
SECRET_NAMES = {".env", ".env.local", "pair-code", "pepper", "install.key", "aerona.key",
                "id_ed25519", "service-account.json", "credentials", ".netrc"}
SECRET_SUFFIXES = (".pem", ".p12", ".pfx", ".key")
DB_SUFFIXES = (".db", ".db-wal", ".db-shm", ".sqlite", ".sqlite3")

REQUIRED = [
    "main.py",
    "requirements.txt",
    "README.md",
    "SECURITY.md",
    "tests/test_launcher.py",
    "dashboard/dist/index.html",
    # Both halves of what makes a deep link work: `shell.html` is the document `/guild/<id>/…`
    # serves (without it the client hydrates a guild page against the prerendered server list and
    # React silently remounts), and `theme-boot.js` is the theme decision, which cannot be inline
    # because the CSP on every response is `script-src 'self'`. A build that predates them ships a
    # panel that loads and misbehaves, which is precisely the "surprising" archive this list refuses.
    "dashboard/dist/shell.html",
    "dashboard/dist/theme-boot.js",
    "dashboard/dist/robots.txt",
    "dashboard/dist/sitemap.xml",
    "scripts/verify-panel.sh",
]


def sh(cmd: list[str]) -> str:
    try:
        return subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, timeout=60).stdout.strip()
    except Exception:  # a missing git or a binary that will not run here is a note, not a crash
        return ""


def workspace_version() -> str:
    toml = ROOT / "Cargo.toml"
    inside = False
    for line in toml.read_text(encoding="utf-8").splitlines():
        if line.strip().startswith("[workspace.package]"):
            inside = True
            continue
        if inside and line.strip().startswith("version"):
            return line.split("=", 1)[1].strip().strip('"')
        if inside and line.strip().startswith("["):
            break
    raise SystemExit("no [workspace.package] version in Cargo.toml")


def platform_tag() -> str:
    system = platform.system().lower()
    machine = platform.machine().lower()
    arch = {"x86_64": "x86_64", "amd64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}[machine]
    return {"darwin": f"macos-{arch}", "windows": f"windows-{arch}.exe"}.get(system, f"linux-{arch}")


def md5(path: Path) -> str:
    h = hashlib.md5()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def walk() -> tuple[list[Path], list[str], int]:
    """Files that belong in the release, plus refusals, plus the repo's own byte count."""
    keep: list[Path] = []
    problems: list[str] = []
    total = 0
    for dirpath, dirnames, filenames in os.walk(ROOT):
        d = Path(dirpath)
        rel = d.relative_to(ROOT)
        dirnames[:] = sorted(x for x in dirnames if x not in EXCLUDE_DIRS)
        for name in sorted(filenames):
            p = d / name
            if not p.is_file() or p.is_symlink():
                continue
            r = Path(*rel.parts, name) if rel.parts else Path(name)
            total += p.stat().st_size
            if name in SECRET_NAMES or name.endswith(SECRET_SUFFIXES):
                problems.append(f"secret-looking file in the tree: {r} — remove it or move it outside the repo")
                continue
            if name in {"pairs.txt"} or (r.parts and r.parts[0] == "data"):
                continue
            if name.endswith(DB_SUFFIXES):
                problems.append(f"runtime database in the source tree: {r} — the archive ships schema, not data")
                continue
            if name.endswith((".log", ".pyc", ".swp", ".orig")) or name.startswith(".DS_Store"):
                continue
            keep.append(p)
    return keep, problems, total


def main() -> int:
    version = workspace_version()
    plat = platform_tag()
    out = PARENT / f"aerona-v{version}-{plat}.zip"
    binary = ROOT / "bin" / f"aerona-{plat}"

    keep, problems, repo_bytes = walk()
    if repo_bytes > MAX_BYTES or len(keep) > MAX_ENTRIES:
        problems.append(f"repo over budget: {repo_bytes:,} bytes / {len(keep)} files "
                        f"(limit {MAX_BYTES:,} bytes, {MAX_ENTRIES} files)")
    for rel in REQUIRED:
        if not (ROOT / rel).exists():
            problems.append(f"required release file missing: {rel}")
    # The claim this archive exists to keep is "no build tooling required", and the only way to
    # keep it is to ship the compiled panel. One hashed stylesheet is the smallest proof that
    # `npm run build` ran in this tree — an index.html alone can be a stale copy from a commit.
    if not list((ROOT / "dashboard" / "dist" / "assets").glob("*.css")):
        problems.append("dashboard/dist/assets holds no built stylesheet — run: npm --prefix dashboard run build")
    if not binary.exists():
        problems.append(f"no prebuilt binary at bin/aerona-{plat} — run: sh scripts/build-static.sh")
    elif not os.access(binary, os.X_OK):
        # The most common way this happens is not a bad build but a restored tree: a snapshot, a
        # `git clone` onto a filesystem without modes, or an unzip that skipped the attributes, all
        # leave the bytes intact and the exec bit off — and a panel that will not start is what the
        # operator discovers. So say which knob to turn.
        problems.append(f"bin/aerona-{plat} is not executable — chmod +x it (a copied or restored tree "
                        f"loses the mode bit, and shipping that would ship an install that cannot start)")
    elif sh([str(binary), "--version"]) == "":
        problems.append(f"bin/aerona-{plat} did not answer --version; it is not runnable here")
    if problems:
        print("refusing to write the archive:", file=sys.stderr)
        for p in problems:
            print(f"  - {p}", file=sys.stderr)
        return 1

    stamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    # A delivered snapshot is not necessarily a checkout: `bin/` was built by
    # scripts/build-static.sh, which embeds the commit hash when it can see one and says
    # nothing when it cannot. Same here - naming a commit that does not exist locally would
    # be worse than saying the archive came from a tree with no history.
    git = sh(["git", "rev-parse", "--short", "HEAD"])
    rev = f"git {git}" + (" (dirty tree)" if sh(["git", "status", "--porcelain"]) else "") if git else "a source tree with no git history in it"
    dirty = False
    info = "\n".join([
        f"Aerona {version} — {plat} release archive",
        f"built {stamp} from {rev}",
        "",
        "Run it:",
        "    python3 main.py                # first boot writes .env, migrates, prints a pairing code",
        "    python3 main.py check          # configuration and schema, nothing started",
        "    python3 main.py healthcheck    # for a container or panel health probe",
        "",
        "Verify it (needs no toolchain, only curl and the shipped binary):",
        "    sh scripts/verify-panel.sh",
        "",
        "Contents:",
        "    main.py, start.sh, start.bat   the launcher — the only thing you execute",
        "    crates/                          the Rust workspace, if you want to rebuild",
        "    dashboard/dist/                  the prerendered site: no npm, no node, already built",
        "    dashboard/src/                   its source, so a fork can change the tokens",
        "    bin/aerona-" + plat + "           statically linked, no libc assumption on the host",
        "",
        f"    binary md5  {md5(binary)}",
        f"    entries     {len(keep) + 1}   (this file included)",
        "",
        "This archive is a snapshot, not an update channel. Nothing phones home; there is no",
        "telemetry to disable and no licence to accept. MIT, see LICENSE.",
        "",
    ]).encode("utf-8")

    out.parent.mkdir(parents=True, exist_ok=True)
    if out.exists():
        out.unlink()
    arcname_prefix = f"aerona-v{version}-{plat}"
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for p in keep:
            rel = p.relative_to(ROOT)
            z.write(p, f"{arcname_prefix}/{rel}")
        z.writestr(f"{arcname_prefix}/RELEASE-INFO.txt", info)

    size = out.stat().st_size
    digest = md5(out)
    out.with_suffix(".zip.md5").write_text(f"{digest}  {out.name}\n", encoding="utf-8")
    if size > MAX_BYTES:
        print(f"archive over budget: {size:,} bytes > {MAX_BYTES:,}", file=sys.stderr)
        return 1

    names = zipfile.ZipFile(out).namelist()
    print(f"wrote {out}")
    print(f"  {len(names)} entries, {size:,} bytes ({size / 1024 / 1024:.1f} MB), md5 {digest}")
    print(f"  repo {repo_bytes:,} bytes in {len(keep)} files; {rev}")
    for rel in REQUIRED:
        mark = "ok " if f"{arcname_prefix}/{rel}" in names else "MISS"
        print(f"  [{mark}] {rel}")
    print(f"  binary: {sh([str(binary), '--version'])}  md5 {md5(binary)}")
    print(f"  dashboard files: {sum(1 for n in names if '/dashboard/dist/' in n)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
