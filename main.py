#!/usr/bin/env python3
"""Aerona launcher — one file, one command, one process.

    python3 main.py            # everything: get a binary, then serve
    python3 main.py build      # only the "get a binary" part
    python3 main.py check      # config + database + dashboard, bind nothing
    python3 main.py doctor     # check, plus the port and Discord's answer to the token
    python3 main.py key create # mint an API key through the running API
    python3 main.py --help

# Why this file exists

The product itself is two Rust binaries: `aerona` (the one you want: dashboard, API and
Discord worker in a single process) and the split pair `aerona-api` + `aerona-bot`.
Neither needs python at run time. What python *is* needed for is the ten minutes around
the first boot, and it keeps being needed because the most common host for this project
is a game panel, which is spectacularly bad at exactly those ten minutes:

* it has a python and nothing else — no rust toolchain, no node, no package manager you
  are allowed to use;
* it runs one command, and an exit looks like a crash, so "run `cargo build` first" is
  not a sentence an operator can act on;
* it wipes `~/.cargo` between restarts and keeps only the server directory;
* it hands you one allocated port and does not tell your config about it.

So this file does the awkward parts and nothing else. In order: it makes sure a binary
exists — preferring the prebuilt one that ships in `bin/`, and otherwise installing a
Rust toolchain *into the server directory* and building — then it starts one process,
restarts it with a backoff if it dies, relays its log with secrets removed, and shuts it
down on SIGTERM so SQLite closes cleanly.

What it deliberately does **not** do: no request handling, no reimplementation of any
rule. Auth, rate limits, validation, Discord behaviour — all in Rust. If you want to
know why a request was refused, read `crates/aerona-api`; if you want to know why the
process is not up, read this file.
"""

from __future__ import annotations

import argparse
import http.cookiejar
import json
import os
import platform
import re
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent
EXE = ".exe" if os.name == "nt" else ""
BIN_NAME = f"aerona{EXE}"
LEGACY_NAMES = ("aerona-api", "aerona-bot")

# A prebuilt binary lives in `bin/` next to this file, named for the platform it runs
# on, so one archive serves every box. `bin/aerona` is the fallback for whoever builds a
# package for exactly one platform.
PREBUILT_DIRS = ("bin", ".")
BUILT_DIRS = ("target/release", "target/debug")

STABLE_SECS = 20.0  # a child up this long counts as healthy, so the backoff resets
# The OAuth callback this install answers on, and therefore the path half of the
# `redirect_uri` it registers with Discord. `crates/aerona-api/src/routes/mod.rs` mounts it;
# `scripts/verify-panel.sh` asks the running API for the same string, so a drift here and
# there is a failed self-test rather than a silent one.
DISCORD_CALLBACK_PATH = "/auth/callback"
MAX_BACKOFF = 60.0
SHUTDOWN_GRACE = 8.0
TOKEN_MIN = 40
ENV_PREFIX = "AERONA_"
PROBE_TIMEOUT = 12.0
RUSTUP_URL = "https://static.rust-lang.org/rustup/dist/{triple}/rustup-init{exe}"
TOOLCHAIN_HINT = "https://sh.rustup.rs"

SECRET_KEYS = ("TOKEN", "SECRET", "KEY", "CODE")
KEY_SCOPES = ("read", "moderate", "config", "tickets")

OFF_VALUES = ("off", "false", "0", "no", "none", "disabled")

# The `.env` we write on a first boot when there is no `.env.example` to copy (a
# binary-only deployment: someone unzipped `bin/` and `dashboard/` and nothing else).
MINIMAL_ENV = """# Aerona — written by main.py on first boot, because there was no .env and
# no .env.example to copy. Everything here is either a default or generated.
AERONA_TOKEN={token}
AERONA_PORT={port}
AERONA_MASTER_KEY={key}
AERONA_BOT={bot}
"""


# --------------------------------------------------------------------- output


def log(component: str, message: str, *, level: str = "info") -> None:
    """One line, prefixed with a timestamp and the component. Machine-parseable on
    purpose: `journalctl -f | grep ' aerona '` must work, and the launcher's own
    chatter must be distinguishable from the child's output."""
    stamp = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    line = f"{stamp} {level:<5} {component:<6} | {message}"
    print(redact(line), flush=True)


def redact(text: str) -> str:
    """Never echo a secret, not even in a crash log where the environment is dumped."""
    out = text
    for key, value in os.environ.items():
        if not key.startswith(ENV_PREFIX) and "SIGHTENGINE" not in key:
            continue
        if not any(part in key for part in SECRET_KEYS):
            continue
        if len(value) >= 6:
            out = out.replace(value, value[:4] + "…[redacted]")
    return re.sub(r"(aer_live_)[A-Za-z0-9_]{8,}", r"\1…[redacted]", out)


# ---------------------------------------------------------------- .env parsing


def unquote(value: str) -> str:
    """Mirror of `aerona_core::config::unquote`, and for the same reason.

    Single quotes are literal; double quotes carry `\\n` / `\\t` / `\\"` escapes; a comment
    may follow the closing quote. Keeping the two implementations alike matters because
    the launcher and the binary read the same file: a value the launcher understood
    differently would decide "is the port free" about a port the server never binds.
    """
    if value[:1] not in ("'", '"'):
        return value.split(" #", 1)[0].rstrip() if " #" in value else value
    quote = value[0]
    end = value.find(quote, 1)
    if end == -1:
        return value
    inner = value[1:end]
    if quote == '"':
        return inner.replace("\\n", "\n").replace("\\t", "\t").replace('\\"', '"')
    return inner


def parse_env_file(path: Path) -> dict[str, str]:
    """A deliberately small subset of dotenv syntax: `KEY=value`, `export KEY=value`,
    comments, blank lines, optional single or double quotes. Anything fancier should be
    set in the environment instead — a launcher is not a shell."""
    values: dict[str, str] = {}
    if not path.is_file():
        return values
    for raw in path.read_text(encoding="utf-8", errors="replace").splitlines():
        line = line_strip = raw.strip()
        if not line_strip or line_strip.startswith("#"):
            continue
        if line_strip.startswith("export "):
            line = line_strip[len("export ") :].strip()
        else:
            line = line_strip
        if "=" not in line:
            continue
        key, _, value = line.partition("=")
        key = key.strip()
        value = unquote(value.strip())
        if re.fullmatch(r"[A-Z][A-Z0-9_]*", key):
            values[key] = value
    return values


def load_env() -> dict[str, str]:
    """`.env` fills in what the environment does not already provide: a panel's
    `EnvironmentFile=`, an egg variable or a shell export always wins, which is what
    makes container use predictable. `.env.local` overrides both, for a developer."""
    env = {k: v for k, v in os.environ.items()}
    for key, value in parse_env_file(ROOT / ".env").items():
        env.setdefault(key, value)
    for key, value in parse_env_file(ROOT / ".env.local").items():
        env[key] = value
    os.environ.update({k: v for k, v in env.items() if k not in os.environ})
    return env


class Config:
    """What the launcher needs to know. The *rules* live in `aerona_core::config`; this
    is the same reading of the same variables, kept to the handful that change what this
    file does (which port to check, whether a token is expected, where things live)."""

    def __init__(self, env: dict[str, str], dev: bool = False) -> None:
        self.env = env
        self.dev = dev
        self.bot_off = (env.get("AERONA_BOT", "") or "").strip().lower() in OFF_VALUES
        self.token = env.get("AERONA_TOKEN", "").strip()
        # A game panel allocates the port and calls it `PORT`. Honouring it is the
        # difference between "starts on the panel" and "the operator has to know about
        # AERONA_PORT before their first boot", which is a silly thing to require.
        self.port_raw = (env.get("AERONA_PORT") or env.get("PORT") or "8080").strip()
        # Left empty when unset, on purpose. The API has a documented fallback for an
        # install that has not named its origin yet (it accepts the host the operator
        # actually types), and inventing `http://localhost:PORT` here would switch that
        # fallback off in the generated `.env` — turning every browser mutation on a game
        # panel's IP into a 403 from a guard rail nobody asked for.
        self.base_url = (env.get("AERONA_BASE_URL") or "").strip()
        # Where *this script* talks to, for its own calls and for the line it prints.
        self.origin = self.base_url or f"http://127.0.0.1:{self.port}"
        self.data_dir = self._abs(env.get("AERONA_DATA_DIR", "data"))
        self.db = self._abs(env.get("AERONA_DB") or str(self.data_dir / "aerona.db"))
        self.static_dir = self._abs(env.get("AERONA_STATIC_DIR", "dashboard/dist"))

    def _abs(self, value: str) -> Path:
        path = Path(value)
        return path if path.is_absolute() else ROOT / path

    @property
    def port(self) -> int:
        try:
            return int(self.port_raw)
        except (TypeError, ValueError):
            return -1

    @property
    def token_required(self) -> bool:
        return not self.bot_off

    def problems(self) -> list[str]:
        """Everything that must be true before a port is bound. Each item is phrased so
        the operator can act on it without reading this file."""
        out: list[str] = []
        if self.token_required and len(self.token) < TOKEN_MIN:
            out.append(
                "AERONA_TOKEN is missing or shorter than "
                f"{TOKEN_MIN} characters — set it in .env (or in the panel's environment), "
                "or run the dashboard alone with AERONA_BOT=off."
            )
        if self.token and re.fullmatch(r"[A-Za-z0-9_\-\.]{%d,}" % TOKEN_MIN, self.token) is None:
            out.append("AERONA_TOKEN does not look like a bot token (base64url segments separated by dots).")
        if not (1 <= self.port <= 65535):
            out.append(f"AERONA_PORT must be a number between 1 and 65535 (got {self.port_raw!r}).")
        if self.port != -1 and self.port < 1024 and not self.dev:
            out.append(
                f"AERONA_PORT={self.port} needs root on most systems — use a port above 1024 "
                "(a game panel gives you one) or pass --dev."
            )
        if self.base_url.endswith("/"):
            out.append("AERONA_BASE_URL must not end with a slash — it is compared against browser Origin headers.")
        try:
            self.data_dir.mkdir(parents=True, exist_ok=True)
            probe = self.data_dir / ".aerona-write-test"
            probe.write_text("ok", encoding="utf-8")
            probe.unlink()
        except OSError as exc:
            out.append(f"data directory {self.data_dir} is not writable: {exc}")
        return out

    def notes(self) -> list[str]:
        """Things worth saying that are not reasons to refuse to start."""
        out: list[str] = []
        if self.bot_off:
            out.append(
                "AERONA_BOT=off: the dashboard serves without a Discord worker. Anything an "
                "operator queues in the outbox waits until a worker connects."
            )
        if self.base_url.startswith("http://") and not any(
            h in self.base_url for h in ("localhost", "127.0.0.1", ".local")
        ):
            # A note, not a refusal, and the wording is deliberately specific. Refusing to
            # boot here was wrong twice over: a game panel hands out one raw TCP port with
            # nothing in front of it, so plain http on a public name is the *normal* shape
            # of an install, not a misconfiguration; and the reason this script gave for
            # refusing it was false — the origin check accepts a browser precisely because
            # this value matches what the address bar says, and `cookies_secure()` on the
            # Rust side already leaves `Secure` off so the session cookie stays sendable.
            # What is actually true is that the session cookie — and with it every
            # moderation action it authorises — crosses the network in clear text, so say
            # that, and say how to fix it.
            out.append(
                f"serving plain http on a public address ({self.base_url}): anyone on the path can read the "
                "session cookie, and therefore every action it authorises. Fine to try, not to keep — put TLS in "
                "front (a Cloudflare Tunnel needs no inbound port), or keep it to `ssh -L "
                f"{self.port}:127.0.0.1:{self.port}` and browse http://localhost:{self.port}."
            )
        if not self.env.get("AERONA_BASE_URL"):
            out.append(
                "AERONA_BASE_URL is unset, so the dashboard accepts its own IP/localhost origin "
                "only. Point a domain at this box? Set AERONA_BASE_URL to exactly what the browser "
                "types, no trailing slash — otherwise Discord's OAuth callback and every mutation "
                "from that domain are refused."
            )
        # The Discord half, said before the first login instead of after the first failed
        # one. `Invalid OAuth2 redirect_uri` is rendered by Discord, in the user's browser,
        # from a request that never reaches this install — so there is no line in any log of
        # ours that explains it. The only useful thing to do is print the exact string that
        # will be sent, and where to paste it.
        client_id = (self.env.get("AERONA_DISCORD_CLIENT_ID") or "").strip()
        client_secret = (self.env.get("AERONA_DISCORD_CLIENT_SECRET") or "").strip()
        if client_id and not client_secret:
            out.append(
                "AERONA_DISCORD_CLIENT_ID is set but AERONA_DISCORD_CLIENT_SECRET is not: nobody can sign in — "
                "Discord is the only door the dashboard has. The secret is the second field under OAuth2 in the portal — and if it was "
                "ever shown in error, press Reset Secret before using it."
            )
        if client_id and client_secret:
            pinned = (self.env.get("AERONA_OAUTH_REDIRECT_URI") or "").strip()
            if pinned:
                out.append(
                    f"Discord login will send redirect_uri={pinned} (pinned by AERONA_OAUTH_REDIRECT_URI, and reused "
                    "verbatim when exchanging the code). Discord compares it character for character with "
                    "OAuth2 → Redirects in the portal: same scheme, same port, same trailing slash or none. Paste "
                    "exactly this string, then press Save Changes — an added-but-unsaved entry does not exist."
                )
            else:
                where = self.base_url if self.env.get("AERONA_BASE_URL") else "the origin in the browser's own address bar"
                out.append(
                    f"Discord login derives its redirect_uri from {where}{DISCORD_CALLBACK_PATH}, so the portal needs "
                    "one entry per address people actually type — `http://127.0.0.1:25154` and `http://localhost:25154` "
                    "are two different origins to Discord, and a port change means a new entry. Set "
                    "AERONA_OAUTH_REDIRECT_URI to pin one string, and read it back on the panel's /login page or with "
                    "`aerona doctor` before blaming the bot."
                )
        if (
            self.env.get("AERONA_MASTER_KEY", "") == ""
            and self.token_required is False
            and self.db.is_file()
        ):
            out.append(
                "no AERONA_MASTER_KEY and no token: the first boot wrote data/install.key, which "
                "is the pepper for every session and API key. Back it up with the database."
            )
        return out

    def child_env(self, component: str) -> dict[str, str]:
        env = dict(os.environ)
        env.setdefault("AERONA_LOG_LEVEL", "debug" if self.dev else "info")
        env["AERONA_COMPONENT"] = component
        # Absolute paths, because the child's cwd is this file's directory and a relative
        # path in a container is a hunt for a mystery file.
        env["AERONA_DB"] = str(self.db)
        env["AERONA_DATA_DIR"] = str(self.data_dir)
        env["AERONA_PORT"] = str(self.port)
        env["AERONA_STATIC_DIR"] = str(self.static_dir)
        if self.bot_off:
            env["AERONA_BOT"] = "off"
        if self.dev:
            env["AERONA_ENV"] = "development"
        return env


def ensure_env_file(config: Config) -> Path | None:
    """Write a first-boot `.env` from `.env.example`, with a generated master key.

    Only when `.env` does not exist, and only the values this launcher can know. The
    token is left as the template's placeholder on purpose: a made-up token fails at
    `aerona doctor` with "Discord refused this token", which is a better lesson than a
    mystery value silently working.
    """
    path = ROOT / ".env"
    if path.exists():
        return None
    template = ROOT / ".env.example"
    key = secrets.token_hex(32)
    if template.is_file():
        lines = []
        for line in template.read_text(encoding="utf-8", errors="replace").splitlines():
            if line.startswith("AERONA_PORT="):
                line = f"AERONA_PORT={config.port}"
            elif line.startswith("AERONA_BASE_URL="):
                value = line.split("=", 1)[1].strip()
                host = value.split("://", 1)[-1].split("/", 1)[0] if value else ""
                if not value or host in ("localhost", "127.0.0.1") or host.startswith("localhost:"):
                    # A placeholder that *looks* like a real origin is worse than no value:
                    # while AERONA_BASE_URL is empty the API accepts the host you type, and
                    # the moment it is set to something else every mutation from the panel's
                    # IP address is refused as cross-origin. Blank it and say why.
                    lines.append(
                        "# Left blank by main.py on purpose. While it is unset the API accepts "
                        "the host you"
                    )
                    lines.append(
                        "# actually browse to (a panel IP, a localhost tunnel). Set it as soon "
                        "as you have"
                    )
                    lines.append("# a domain — Discord's OAuth callback and browser Origin checks then require the exact match.")
                    line = "AERONA_BASE_URL="
                # Anything else was written deliberately; keep it.
            elif line.startswith("AERONA_MASTER_KEY="):
                line = f"AERONA_MASTER_KEY={key}"
            elif line.startswith("AERONA_BOT=") and config.bot_off:
                line = "AERONA_BOT=off"
            elif line.startswith("AERONA_TOKEN=") and config.bot_off:
                line = "# AERONA_TOKEN= — not needed while AERONA_BOT=off"
            lines.append(line)
        text = "\n".join(lines) + "\n"
    else:
        text = MINIMAL_ENV.format(
            port=config.port,
            key=key,
            token="" if config.bot_off else "put-your-discord-bot-token-here",
            bot="off" if config.bot_off else "",
        )
    try:
        path.write_text(text, encoding="utf-8")
        path.chmod(0o600)
    except OSError as exc:
        log("env", f"could not write {path}: {exc}", level="warn")
        return None
    log(
        "env",
        f"wrote {path} (chmod 600) from {template.name if template.is_file() else 'a built-in template'}, "
        "with a generated AERONA_MASTER_KEY — fill in AERONA_TOKEN and restart",
    )
    return path


# ------------------------------------------------------------------ platform


def platform_tag() -> str:
    """The suffix `bin/aerona-<tag>` is built with. Deliberately coarse: OS and word
    size, because that is all that has to differ for a static binary to run."""
    system = {"Linux": "linux", "Darwin": "macos", "Windows": "windows"}.get(
        platform.system(), platform.system().lower()
    )
    machine = (platform.machine() or "").lower()
    arch = {
        "x86_64": "x86_64",
        "amd64": "x86_64",
        "i686": "x86_64",  # a 32-bit kernel on 64-bit hardware is rare enough to ignore
        "i386": "x86",
        "aarch64": "aarch64",
        "arm64": "aarch64",
        "armv7l": "armv7",
        "ppc64le": "powerpc64le",
    }.get(machine, machine or "unknown")
    return f"{system}-{arch}"


def rustup_triple() -> str | None:
    """The rustup-init asset for this machine, or None if we do not know (in which case
    the operator installs the toolchain themselves and we say so)."""
    system = platform.system()
    machine = (platform.machine() or "").lower()
    if system == "Linux":
        return {
            "x86_64": "x86_64-unknown-linux-gnu",
            "amd64": "x86_64-unknown-linux-gnu",
            "aarch64": "aarch64-unknown-linux-gnu",
            "arm64": "aarch64-unknown-linux-gnu",
            "armv7l": "armv7-unknown-linux-gnueabihf",
            "i686": "i686-unknown-linux-gnu",
            "i386": "i686-unknown-linux-gnu",
        }.get(machine)
    if system == "Darwin":
        return {"x86_64": "x86_64-apple-darwin", "arm64": "aarch64-apple-darwin"}.get(machine)
    if system == "Windows":
        return {"amd64": "x86_64-pc-windows-msvc", "x86_64": "x86_64-pc-windows-msvc"}.get(machine)
    return None


def binary_names(stem: str) -> list[str]:
    """Everything `bin/` might call this program, best match first."""
    tag = platform_tag()
    out = [f"{stem}-{tag}{EXE}", f"{stem}-{tag}", f"{stem}{EXE}", stem]
    if EXE:
        out.append(f"{stem}.exe")
    # Deduplicate while keeping order.
    seen: set[str] = set()
    return [n for n in out if not (n in seen or seen.add(n))]


# -------------------------------------------------------------- binary lookup

_PROBED: dict[str, bool] = {}


def candidate_paths(stem: str, *, include_built: bool = True) -> list[Path]:
    dirs = list(PREBUILT_DIRS) + (list(BUILT_DIRS) if include_built else [])
    out: list[Path] = []
    for directory in dirs:
        for name in binary_names(stem):
            path = ROOT / directory / name
            if path not in out:
                out.append(path)
    on_path = shutil.which(stem)
    if on_path:
        out.append(Path(on_path))
    return out


def usable(path: Path) -> bool:
    """Is this a binary we can actually run? `--version` is the whole test: it catches a
    wrong-architecture ELF (Exec format error), a zip that lost its exec bit, a file that
    is a symlink to nothing, and a build from a different major version — none of which a
    permission bit or a magic-number sniff would notice."""
    if not path.is_file():
        return False
    key = f"{path}:{path.stat().st_size}:{int(path.stat().st_mtime)}"
    if key in _PROBED:
        return _PROBED[key]
    ok = False
    if not os.access(path, os.X_OK):
        try:
            path.chmod(path.stat().st_mode | 0o755)
            log("bin", f"set the execute bit on {path.name} (a zip or a panel upload often drops it)")
        except OSError as exc:
            log("bin", f"{path} is not executable and could not be fixed: {exc}", level="warn")
    if os.access(path, os.X_OK):
        try:
            res = subprocess.run(  # noqa: S603 — our own path, our own file
                [str(path), "--version"],
                cwd=str(ROOT),
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                stdin=subprocess.DEVNULL,
                timeout=PROBE_TIMEOUT,
                check=False,
            )
            ok = res.returncode == 0 and b"aerona" in (res.stdout or b"")
            if not ok:
                detail = (res.stdout or b"").decode("utf-8", "replace").strip().splitlines()
                log("bin", f"{path.name} --version said {res.returncode}: {detail[0] if detail else ''}", level="warn")
        except OSError as exc:
            log(
                "bin",
                f"{path.name} cannot be run here ({exc.__class__.__name__}: {exc}) — this box needs a different binary",
                level="warn",
            )
        except subprocess.TimeoutExpired:
            log("bin", f"{path.name} did not answer `--version` in {PROBE_TIMEOUT:.0f}s", level="warn")
    _PROBED[key] = ok
    return ok


def find_unified() -> Path | None:
    for path in candidate_paths("aerona"):
        if usable(path):
            return path
    return None


def find_legacy() -> tuple[Path, Path] | None:
    """The two-process pair, for an install that predates the unified binary (or a
    supervisor that wants them apart). Both must be present: half a pair is worse than
    none, because the dashboard would come up with no worker and no queue drained."""
    found = []
    for stem in LEGACY_NAMES:
        hit = next((p for p in candidate_paths(stem) if usable(p)), None)
        if hit is None:
            return None
        found.append(hit)
    return found[0], found[1]


# ----------------------------------------------------------------- toolchain


def cache_dir() -> Path:
    """Where a toolchain we installed ourselves goes. Inside the project, because a panel
    keeps the server directory and throws away the home directory."""
    override = os.environ.get("AERONA_TOOLCHAIN_DIR", "").strip()
    return Path(override) if override else ROOT / ".cache"


def cargo_binary() -> Path | None:
    """The cargo to build with, or None when no toolchain is installed anywhere.

    A toolchain this launcher installed itself wins over one that merely happens to be on
    `PATH`. `toolchain_env()` points `RUSTUP_HOME` and `CARGO_HOME` at the local `.cache`
    directories, so a rustup shim picked up from elsewhere would look for its toolchains in
    an empty local `rustup/` and die with "no installed targets" — a confusing failure for
    the one user who was doing nothing wrong. A cargo already on `PATH` is still used when
    the launcher has not installed anything, which is the ordinary developer case.
    """
    local = cache_dir() / "cargo" / "bin" / f"cargo{EXE}"
    if local.is_file():
        return local
    found = shutil.which("cargo")
    return Path(found) if found else None


def toolchain_env() -> dict[str, str]:
    """Environment for a cargo invocation: the local toolchain first on PATH, and its
    caches inside the project so they survive a restart."""
    env = dict(os.environ)
    root = cache_dir()
    cargo_bin = root / "cargo" / "bin"
    rustup = root / "rustup"
    env.setdefault("RUSTUP_HOME", str(rustup))
    env.setdefault("CARGO_HOME", str(root / "cargo"))
    if cargo_bin.is_dir():
        env["PATH"] = os.pathsep.join([str(cargo_bin), env.get("PATH", "")])
    env["CARGO_TERM_COLOR"] = env.get("CARGO_TERM_COLOR", "never")
    env["CARGO_NET_RETRY"] = env.get("CARGO_NET_RETRY", "3")
    env.setdefault("RUST_BACKTRACE", "1")
    return env


def download(url: str, dest: Path) -> bool:
    """Fetch one file, logging progress every few MB so a panel's log shows life during a
    15 MB pull on a shared uplink. Returns False on any failure, with the reason logged."""
    try:
        request = urllib.request.Request(url, headers={"user-agent": "aerona-launcher/1"})
        with urllib.request.urlopen(request, timeout=60) as response:  # noqa: S310 — https constant
            total = int(response.headers.get("content-length") or 0)
            dest.parent.mkdir(parents=True, exist_ok=True)
            done = 0
            next_mark = 4 * 1024 * 1024
            with dest.open("wb") as handle:
                while True:
                    chunk = response.read(256 * 1024)
                    if not chunk:
                        break
                    handle.write(chunk)
                    done += len(chunk)
                    if done >= next_mark:
                        size = f"{done / 1048576:.0f} MB" + (f" of {total / 1048576:.0f} MB" if total else "")
                        log("rust", f"downloaded {size}")
                        next_mark += 4 * 1024 * 1024
    except (urllib.error.URLError, OSError, ValueError) as exc:
        log("rust", f"could not download {url}: {exc}", level="error")
        log("rust", f"if this box has no egress, install Rust yourself: curl {TOOLCHAIN_HINT} | sh", level="warn")
        return False
    if dest.stat().st_size < 500_000:
        log("rust", f"{url} came back at {dest.stat().st_size} bytes — that is not a toolchain installer", level="error")
        dest.unlink(missing_ok=True)
        return False
    return True


def install_rust() -> Path | None:
    """Install a minimal stable toolchain into `AERONA_TOOLCHAIN_DIR` (default
    `.cache/`), using rustup itself, and return the cargo binary.

    This is the whole reason `python3 main.py` works on a box with nothing on it. It is
    stdlib-only, it never touches anything outside the project, and it is skipped when
    `AERONA_NO_AUTOINSTALL=1` — because on some hosts downloading a compiler at boot is
    exactly what the operator does not want, and then the right answer is the prebuilt
    binary in `bin/`, not a surprise 15 MB fetch.
    """
    if os.environ.get("AERONA_NO_AUTOINSTALL", "").strip().lower() in ("1", "true", "yes"):
        log("rust", "AERONA_NO_AUTOINSTALL is set, so I will not install a toolchain", level="error")
        log("rust", "put a prebuilt `bin/aerona-%s` in place, or unset that variable" % platform_tag(), level="warn")
        return None
    triple = rustup_triple()
    if triple is None:
        log("rust", f"I do not know a rustup build for {platform.system()}/{platform.machine()}", level="error")
        log("rust", f"install Rust by hand (<{TOOLCHAIN_HINT}>) and run me again", level="warn")
        return None

    root = cache_dir()
    cargo_bin = root / "cargo" / "bin" / f"cargo{EXE}"
    if cargo_bin.is_file():
        log("rust", f"reusing the toolchain in {root}")
        return cargo_bin

    log("rust", f"no cargo found: installing a minimal stable toolchain for {triple} into {root}")
    log("rust", "one-time cost, about 15 MB; nothing outside this directory is touched")
    installer = root / f"rustup-init{EXE}"
    if not download(RUSTUP_URL.format(triple=triple, exe=EXE), installer):
        return None
    try:
        installer.chmod(0o700)
    except OSError:
        pass
    env = toolchain_env()
    # `-y` plus `--no-modify-path`: we never edit a shell profile we did not create, and
    # `minimal` because rustfmt and clippy are a developer's tools, not a server's.
    cmd = [
        str(installer),
        "-y",
        "--profile",
        "minimal",
        "--default-toolchain",
        "stable",
        "--no-modify-path",
    ]
    log("rust", "running the rustup installer")
    if subprocess.run(cmd, cwd=str(root), env=env, check=False).returncode != 0:  # noqa: S603
        log("rust", "the rustup installer failed — its output above says why", level="error")
        return None
    installer.unlink(missing_ok=True)
    if not cargo_bin.is_file():
        log("rust", f"rustup said it worked but {cargo_bin} is not there", level="error")
        return None
    log("rust", "toolchain installed")
    return cargo_bin


def dashboard_needs_build(config: Config) -> bool:
    return not (config.static_dir / "index.html").is_file()


def build_dashboard(config: Config) -> int:
    """`npm run build` if npm is here, otherwise say what is missing and move on. A
    missing dashboard must never be a reason the API does not come up."""
    dashboard = ROOT / "dashboard"
    if not dashboard.is_dir():
        log("build", "dashboard/ is not in this directory: the API serves the JSON only", level="warn")
        return 0
    if shutil.which("npm") is None:
        log(
            "build",
            "npm is not on PATH: skipping the dashboard. The archive ships a built one in "
            "dashboard/dist — if yours is missing, re-extract the archive with its directories, "
            "or install node and run `npm --prefix dashboard run build`.",
            level="warn",
        )
        return 0
    if not (dashboard / "node_modules").is_dir():
        install = (
            ["npm", "ci", "--no-audit", "--no-fund"]
            if (dashboard / "package-lock.json").is_file()
            else ["npm", "install", "--no-audit", "--no-fund"]
        )
        log("build", "npm install (dashboard) — this needs network and a minute")
        if subprocess.run(install, cwd=str(dashboard), check=False).returncode != 0:  # noqa: S607, S603
            log("build", "npm install failed; the dashboard was not built", level="error")
            return 1
    # The full chain, not just `vite build`: prerender + the SEO audit are part of the
    # build, and a dashboard that passes without them is a broken deliverable.
    log("build", "npm run build (dashboard)")
    if subprocess.run(["npm", "run", "build"], cwd=str(dashboard), check=False).returncode != 0:  # noqa: S607, S603
        log("build", "the dashboard build failed (its output is above)", level="error")
        return 1
    if dashboard_needs_build(config):
        log("build", f"npm said it worked but {config.static_dir / 'index.html'} is still missing", level="error")
        return 1
    return 0


def total_memory_kib() -> int:
    """`MemTotal` in KiB, or 0 when this platform does not say (then we guess nothing)."""
    try:
        with open("/proc/meminfo", encoding="utf-8") as fh:
            for line in fh:
                if line.startswith("MemTotal:"):
                    return int(line.split()[1])
    except (OSError, ValueError, IndexError):
        pass
    return 0


def build_jobs(total_kib: int | None = None) -> str | None:
    """The `-j` to give cargo, chosen from memory rather than from the core count.

    Parallel rustc processes are what kills a small container: `serenity` alone sits near
    800 MB at release settings, so two of them on a 2 GB box with no swap does not build
    slower — it builds *not at all*. The kernel kills the compile, cargo exits non-zero,
    and an operator whose panel restarts on exit gets a crash loop instead of a bot.
    Cores are the wrong input for that trade; RAM is the right one. `AERONA_BUILD_JOBS`
    overrides (set it to `auto` to hand the decision back to cargo).
    """
    override = (os.environ.get("AERONA_BUILD_JOBS") or "").strip()
    if override:
        return None if override.lower() in ("0", "auto", "default") else override
    if total_kib is None:
        total_kib = total_memory_kib()
    if not total_kib:
        return None
    gib = total_kib / (1024 * 1024)
    if gib < 3:
        return "1"
    if gib < 8:
        return "2"
    return None


def cargo_build(config: Config, *, release: bool = True) -> int:
    """Build the workspace with whatever cargo we have or can install. No timeouts here:
    the first release build of this tree is legitimately ten minutes on two vCPUs, and a
    launcher that gives up at five would be worse than no launcher."""
    cargo = cargo_binary()
    if cargo is None:
        cargo = install_rust()
    if cargo is None:
        log(
            "build",
            "no Rust toolchain and I could not install one — put a prebuilt "
            f"`bin/aerona-{platform_tag()}{EXE}` next to this file and run me again",
            level="error",
        )
        return 127
    profile = "release" if release and not config.dev else "debug"
    cmd = [str(cargo), "build", f"--{profile}", "--workspace"]
    jobs = build_jobs()
    why = ""
    if jobs:
        cmd += ["-j", jobs]
        total = total_memory_kib()
        if total:
            why = f" (this box has {total / (1024 * 1024):.1f} GiB of RAM; rustc is memory-hungry)"
    log("build", f"{cargo.name} build --{profile} --workspace" + (f" -j {jobs}" if jobs else ""))
    if why:
        log("build", f"limiting parallelism{why}. Override with AERONA_BUILD_JOBS=n.")
    log("build", "first build compiles every dependency: minutes, not seconds; only once")
    installed_here = cargo.parent == (cache_dir() / "cargo" / "bin")
    if subprocess.run(cmd, cwd=str(ROOT), env=toolchain_env(), check=False).returncode != 0:  # noqa: S603
        log("build", f"cargo build --{profile} failed (see above)", level="error")
        return 1
    if installed_here:
        # A toolchain we downloaded is 600 MB and a build tree is more; on a panel with a
        # disk quota the operator should know that is sitting there.
        log(
            "build",
            f"built. The toolchain and build tree live in {cache_dir()} and target/ "
            "(roughly 2 GB); `rm -rf .cache target` frees it, and target/ is only needed "
            "again if you change the Rust source.",
        )
    return 0


def ensure_runtime(
    config: Config, *, allow_build: bool = True, force_build: bool = False
) -> tuple[str, list[str]]:
    """Return `(kind, argv-prefix)` for the thing `run` should start.

    `kind` is `unified` (one process: dashboard + API + worker — what almost everybody
    wants), `legacy` (the two binaries, for a supervisor that owns restarts) or `none`.
    """
    if not force_build:
        unified = find_unified()
        if unified is not None:
            return "unified", [str(unified)]
        legacy = find_legacy()
        if legacy is not None:
            return "legacy", [str(legacy[0]), str(legacy[1])]

    if not allow_build:
        return "none", []

    # A source checkout with no binaries is the normal case for anyone who cloned rather
    # than unzipped, and the panel case where no build exists for its platform.
    log("bin", f"no usable aerona binary for {platform_tag()}: building one")
    if force_build:
        log("bin", "--force was passed, so cargo will recompile even if a binary exists")
    if cargo_build(config, release=not config.dev) != 0:
        return "none", []
    if dashboard_needs_build(config) and build_dashboard(config) != 0:
        return "none", []
    unified = find_unified()
    if unified is not None:
        return "unified", [str(unified)]
    legacy = find_legacy()
    if legacy is not None:
        log("bin", "the unified binary was not produced; falling back to the two-process pair", level="warn")
        return "legacy", [str(legacy[0]), str(legacy[1])]
    log("bin", "the build finished but no runnable binary turned up", level="error")
    return "none", []


# ---------------------------------------------------------------- pump output


def pump(stream, component: str) -> None:
    """Relay a child's output line by line so interleaving stays readable and `grep`
    still works on the supervisor's stdout. Reading is a blocking call, hence a thread
    per stream; `daemon=True` means a stuck child can never keep the parent alive."""
    try:
        for raw in iter(stream.readline, b""):
            if not raw:
                break
            text = raw.decode("utf-8", "replace").rstrip()
            if text:
                log(component, text)
    except (ValueError, OSError):
        pass  # the pipe closed during shutdown; that is normal
    finally:
        try:
            stream.close()
        except OSError:
            pass


# ------------------------------------------------------------------- process


class Child:
    def __init__(self, name: str, argv: list[str], env: dict[str, str]) -> None:
        self.name = name
        self.argv = argv
        self.env = env
        self.proc: subprocess.Popen[bytes] | None = None
        self.started_at = 0.0
        self.restarts = 0
        self.backoff = 1.0

    @property
    def alive(self) -> bool:
        return self.proc is not None and self.proc.poll() is None

    def start(self) -> None:
        self.proc = subprocess.Popen(  # noqa: S603 — argv is built from our own paths
            self.argv,
            cwd=str(ROOT),
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
            start_new_session=True,  # its own process group, so a kill is not an orphan-maker
        )
        assert self.proc.stdout is not None
        threading.Thread(target=pump, args=(self.proc.stdout, self.name), daemon=True).start()
        self.started_at = time.monotonic()
        shown = " ".join(Path(a).name if a.startswith(str(ROOT)) else a for a in self.argv)
        log(self.name, f"started pid={self.proc.pid} :: {redact(shown)}")

    def stop(self, grace: float = SHUTDOWN_GRACE) -> None:
        if not self.alive or self.proc is None:
            return
        try:
            os.killpg(os.getpgid(self.proc.pid), signal.SIGTERM)
            log(self.name, f"pid={self.proc.pid} sent SIGTERM; draining for {grace:.0f}s")
        except (ProcessLookupError, PermissionError):
            return
        try:
            self.proc.wait(timeout=grace)
            return
        except subprocess.TimeoutExpired:
            pass
        # A child that ignored SIGTERM for eight seconds is not going to listen now, and
        # a SQLite WAL left open by it is a real problem for the next start.
        log(self.name, "did not exit in time; sending SIGKILL", level="warn")
        try:
            os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            log(self.name, "still running after SIGKILL — leaving it to init", level="warn")

    def note_exit(self, code: int | None) -> float:
        """How long to wait before restarting: exponential, reset once the process has
        been up long enough to have done something useful."""
        uptime = time.monotonic() - self.started_at
        if uptime >= STABLE_SECS:
            self.restarts = 0
            self.backoff = 1.0
        else:
            self.restarts += 1
            self.backoff = min(MAX_BACKOFF, self.backoff * 2)
        level = "warn" if uptime < STABLE_SECS else "info"
        log(self.name, f"exited code={code} after {uptime:.1f}s (restarts={self.restarts})", level=level)
        return 0.0 if uptime >= STABLE_SECS else self.backoff


def port_is_free(port: int) -> bool:
    if port <= 0:
        return False
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.settimeout(0.4)
        return sock.connect_ex(("127.0.0.1", port)) != 0


# -------------------------------------------------------------------- checks


def run_binary_check(argv: list[str], config: Config, component: str, *, sub: str = "check") -> int:
    """Ask the binary itself. It runs PRAGMA integrity_check, prints the config summary
    and the install id, and exits — so a corrupt WAL is found before a dashboard is
    exposed, and we never reimplement Rust's rules in python."""
    cmd = [*argv, sub]
    try:
        res = subprocess.run(  # noqa: S603
            cmd,
            cwd=str(ROOT),
            env=config.child_env("check"),
            capture_output=True,
            text=True,
            timeout=120,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        log(component, f"could not run `{Path(cmd[0]).name} {sub}`: {exc}", level="error")
        return 1
    for line in (res.stdout + res.stderr).splitlines():
        log(component, line.strip() or " ", level="info" if res.returncode == 0 else "error")
    return res.returncode


def check(config: Config, *, require_dashboard: bool = True, doctor: bool = False) -> int:
    """`main.py check`: config and files first (python's job), then the binary's own
    verdict (Rust's job). `doctor` adds the network questions."""
    failures = 0
    for item in config.problems():
        log("check", item, level="error")
        failures += 1
    for note in config.notes():
        log("check", note, level="warn")

    # A diagnostic that has to compile first is still a diagnostic you can run on a box
    # with nothing on it, which is the only kind of box where you need a diagnostic.
    kind, argv = ensure_runtime(config)
    if kind == "none":
        log("check", "no aerona binary is available and building one was refused", level="error")
        return 2
    if kind == "unified":
        log("check", f"aerona: {rel(argv[0])}")
    else:
        log("check", f"legacy pair: {rel(argv[0])} + {rel(argv[1])} (prefer the unified binary)")

    if require_dashboard:
        if (config.static_dir / "index.html").is_file():
            routes = sum(1 for p in config.static_dir.glob("*/index.html")) + sum(
                1 for p in config.static_dir.glob("*/*/index.html")
            )
            size = (config.static_dir / "index.html").stat().st_size
            log("check", f"dashboard: built ({size // 1024} KB shell, {routes} prerendered route file(s))")
        else:
            log(
                "check",
                f"dashboard: not built — the API will run api-only (looked in {config.static_dir})",
                level="warn",
            )

    if config.db.is_file():
        size = config.db.stat().st_size
        log("check", f"database: {config.db.name}, {size / 1024:.0f} KB")
        if size == 0:
            log(
                "check",
                f"{config.db} is an empty file — a half-created database. Delete it or point AERONA_DB elsewhere.",
                level="error",
            )
            failures += 1

    if failures:
        log("check", f"refusing to ask the binary: {failures} problem(s) above", level="error")
        return 2
    sub = "doctor" if doctor else "check" if kind == "unified" else "--check"
    if run_binary_check([argv[0]], config, "check", sub=sub) != 0:
        failures += 1
        log("check", f"`{Path(argv[0]).name} {sub}` said no", level="error")
    return 2 if failures else 0


def rel(path: str | Path) -> str:
    p = Path(path)
    try:
        return str(p.relative_to(ROOT))
    except ValueError:
        return str(p)


# ------------------------------------------------------------------ api calls


class ApiDown(Exception):
    pass


def api_opener() -> urllib.request.OpenerDirector:
    """An opener that keeps cookies for one session of calls.

    `urllib.request.urlopen` has no cookie jar: it drops `Set-Cookie`, so a
    session-then-call sequence would arrive at the second endpoint anonymous. The jar
    lives for exactly one `api_with_session` call — this script is not a browser and
    leaves no cookie file behind.
    """
    return urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))


def api_request(
    config: Config,
    method: str,
    path: str,
    *,
    body: dict | None = None,
    opener: urllib.request.OpenerDirector | None = None,
    headers: dict[str, str] | None = None,
) -> tuple[int, dict]:
    """A tiny HTTP client for `key create|revoke`, so an operator does not have to
    hand-roll curl with the right CSRF dance. A CLI gets a session the same way anything
    else does now: it carries one in, by cookie value or by API key.

    No `Origin` header, deliberately: the origin rule on mutations exists to stop a
    *browser* on somebody else's site riding an operator's session, and a CLI is not that —
    it authenticates with the cookie and the `x-aerona-csrf` value or with an API key.
    Sending an Origin here would mean guessing which origin the server counts as its own,
    and guessing wrong on a domain install means the launcher is 403'd by its own guard
    rail while the dashboard works fine.
    """
    url = f"http://127.0.0.1:{config.port}/api/v1{path}"
    sent = {"accept": "application/json", "content-type": "application/json"}
    if headers:
        sent.update(headers)
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method, headers=sent)  # noqa: S310
    if opener is None:
        opener = api_opener()
    try:
        with opener.open(req, timeout=15) as res:
            payload = res.read().decode("utf-8", "replace")
            return res.status, (json.loads(payload) if payload.strip() else {})
    except urllib.error.HTTPError as exc:
        payload = exc.read().decode("utf-8", "replace")
        try:
            return exc.code, json.loads(payload)
        except json.JSONDecodeError:
            return exc.code, {"raw": payload}
    except (urllib.error.URLError, TimeoutError, socket.timeout) as exc:
        raise ApiDown(f"the API is not answering on port {config.port}: {exc}") from exc


def api_with_session(
    config: Config, method: str, path: str, *, body: dict | None = None
) -> tuple[int, dict]:
    """`method path`, on whatever credential this machine can produce.

    Two of them, in this order: `AERONA_SESSION` (the cookie value copied out of a signed-in
    browser) and `AERONA_API_KEY`. There is no third. Redeeming an on-disk pairing code used to
    be here; it went with the code, because a door a browser can walk through and a door a CLI
    can walk through are the same door — and the product has exactly one, which is Discord.
    """
    opener = api_opener()
    # Carried on the *first* call, not just the one that needs it: `/me` is what proves the
    # value is a live session, and a cookie the server would reject has to be named in the
    # error rather than silently traded for a pairing dance that no longer exists.
    carried = (config.env.get("AERONA_SESSION") or "").strip()
    headers = {"cookie": f"aerona_session={carried}"} if carried else None
    status, me = api_request(config, "GET", "/me", opener=opener, headers=headers)
    csrf = me.get("data", {}).get("csrf") if status == 200 else None
    if csrf is not None:
        sent = dict(headers or {})
        sent["x-aerona-csrf"] = csrf
        return api_request(config, method, path, body=body, opener=opener, headers=sent)
    key = (config.env.get("AERONA_API_KEY") or "").strip()
    if key:
        # `/keys*` is session-only by design — a leaked API key must not mint or revoke its
        # siblings — so a key gets as far as the guild routes and no further.
        if path.startswith("/keys"):
            raise ApiDown(
                "the key routes need a dashboard session, not an API key: re-run with "
                f"AERONA_SESSION=<cookie value>, or create the key in the panel at {config.origin}/access/"
            )
        sent = dict(headers or {})
        sent["authorization"] = f"Bearer {key}"
        return api_request(config, method, path, body=body, opener=opener, headers=sent)
    raise ApiDown(
        "no session for this call. Authorise in a browser at "
        f"{config.origin}/login/ — Discord, and the account has to administer a server "
        "in — then re-run with AERONA_SESSION=<the aerona_session cookie>, or put a key in "
        "AERONA_API_KEY"
    )


# -------------------------------------------------------------------- commands


def cmd_run(config: Config, args: argparse.Namespace) -> int:
    ensure_env_file(config)
    problems = config.problems()
    if problems:
        for item in problems:
            log("run", item, level="error")
        log("run", "refusing to start; fix the above (or run `main.py check`)", level="error")
        return 2
    for note in config.notes():
        log("run", note, level="warn")

    kind, argv = ensure_runtime(config, allow_build=not args.no_build, force_build=args.force)
    if kind == "none":
        hint = (
            "put a prebuilt binary at bin/aerona-%s%s, or let me build one (drop --no-build)"
            % (platform_tag(), EXE)
            if args.no_build
            else "see the build errors above"
        )
        log("run", f"nothing to run: {hint}", level="error")
        return 127

    if not port_is_free(config.port) and not args.allow_existing_port:
        log(
            "run",
            f"port {config.port} is already answered by something: one process must own it "
            "(that is the whole point of the shared port)",
            level="error",
        )
        return 3

    children: list[Child] = []
    if kind == "unified":
        extra: list[str] = []
        if args.no_bot:
            extra.append("--no-bot")
        if config.dev:
            extra.append("--dev")
        children.append(Child("aerona", [*argv, "run", *extra], config.child_env("aerona")))
    else:
        children.append(Child("api", [argv[0]], config.child_env("api")))
        if not args.no_bot and not config.bot_off:
            children.append(Child("bot", [argv[1]], config.child_env("bot")))
        else:
            log("run", "--no-bot with the legacy pair: the queue will not drain", level="warn")

    stop = threading.Event()
    install_signal_handlers(stop)

    if args.once:
        # `--once` is for a supervisor of your own (systemd, docker `init`, a panel that
        # restarts by itself) that should own restarts; two layers of backoff only hide
        # the real failure.
        for child in children:
            child.start()
        for child in children:
            while child.alive and not stop.is_set():
                time.sleep(0.4)
        for child in children:
            child.stop()
        return 0

    for child in children:
        child.start()
    log(
        "run",
        f"serving the dashboard and /api/v1 on port {config.port} "
        f"({config.origin}); {len(children)} process(es), Ctrl-C or SIGTERM stops them",
    )

    while not stop.is_set():
        for child in children:
            if child.proc is not None and not child.alive:
                code = child.proc.returncode
                wait = child.note_exit(code)
                if stop.is_set():
                    break
                log(child.name, f"restarting in {wait:.1f}s", level="warn")
                deadline = time.monotonic() + wait
                while time.monotonic() < deadline and not stop.is_set():
                    time.sleep(0.2)
                if not stop.is_set():
                    try:
                        child.start()
                    except OSError as exc:
                        log(child.name, f"could not restart: {exc}", level="error")
                        stop.set()
                        break
        if stop.is_set():
            break
        time.sleep(0.5)

    log("run", "shutting down")
    for child in children:
        child.stop()
    log("run", "stopped")
    return 0


def install_signal_handlers(stop: threading.Event) -> None:
    def handler(signum, _frame):  # noqa: ANN001, ARG001
        if not stop.is_set():
            log("run", f"signal {signal.Signals(signum).name} received")
        stop.set()

    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        try:
            signal.signal(sig, handler)
        except (ValueError, OSError):
            pass  # not the main thread; the caller owns signals
    try:
        signal.signal(signal.SIGPIPE, signal.SIG_DFL)  # type: ignore[attr-defined]
    except (ValueError, OSError, AttributeError):
        pass


def cmd_build(config: Config, args: argparse.Namespace) -> int:
    """`main.py build`: make a binary exist. Idempotent and quiet when one already does,
    because a panel may run this as its install step every single boot."""
    if not args.force:
        unified = find_unified()
        if unified is not None:
            log("build", f"nothing to do: {rel(unified)} is already here and runs")
            if dashboard_needs_build(config):
                log("build", "the dashboard is missing, though — building just that")
                return build_dashboard(config)
            return 0
        legacy = find_legacy()
        if legacy is not None:
            log("build", f"the legacy pair is present ({rel(legacy[0])}, {rel(legacy[1])}): nothing to build")
            return 0
    if args.rust_only:
        return 0 if install_rust() is not None else 1
    if cargo_build(config, release=not args.debug) != 0:
        return 1
    if dashboard_needs_build(config):
        return build_dashboard(config)
    log("build", "dashboard already built — leaving it alone")
    return 0


def cmd_check(config: Config, args: argparse.Namespace) -> int:
    return check(config, require_dashboard=not args.no_dashboard, doctor=args.detailed)


def cmd_healthcheck(config: Config, _args: argparse.Namespace) -> int:
    """Exit 0 when the product is serving. Cheap enough for a cron job or a panel
    health interval, and honest: it asks the API, which asks the database, which is where
    the truth about the bot's last heartbeat lives."""
    try:
        status, health = api_request(config, "GET", "/healthz")
    except ApiDown as exc:
        log("health", str(exc), level="error")
        return 1
    if status != 200:
        log("health", f"/healthz answered {status}: {health}", level="error")
        return 1
    status, res = api_request(config, "GET", "/status")
    data = res.get("data", {}) if status == 200 else {}
    bot = data.get("bot") or {}
    queue = data.get("queue") or {}
    if status != 200:
        log("health", f"api up but /status answered {status}", level="error")
        return 1
    worker_off = (config.env.get("AERONA_BOT", "") or "").strip().lower() in OFF_VALUES
    if bot.get("stale") or (not bot and not worker_off):
        log(
            "health",
            "api is serving but no worker has reported (last: %s) %s"
            % (bot.get("reportingAt"), "— AERONA_BOT=off says that is intended" if worker_off else "— gateway trouble?"),
            level="warn" if worker_off else "error",
        )
        if not worker_off:
            return 1
    log(
        "health",
        f"api ok · {data.get('guilds')} guild(s) · latency {bot.get('wsLatencyMs')} ms · "
        f"queue {queue.get('pending')} pending/{queue.get('failed')} failed · "
        f"db {((data.get('db') or {}).get('sizeKb', 0) / 1024):.1f} MB",
    )
    return 0


def cmd_key(config: Config, args: argparse.Namespace) -> int:
    """Create or revoke an API key through the running API rather than by writing SQL:
    the key is a MAC in the database, and hand-writing a row would produce a key that
    cannot be verified."""
    try:
        if args.action == "create":
            scopes = list(dict.fromkeys(args.scope or ["read"]))
            unknown = [s for s in scopes if s not in KEY_SCOPES]
            if unknown:
                log(
                    "key",
                    f"unknown scope(s) {', '.join(unknown)} — expected any of {', '.join(KEY_SCOPES)}",
                    level="error",
                )
                return 2
            body: dict = {
                "label": args.label or f"cli-{time.strftime('%Y%m%d-%H%M')}",
                "scopes": scopes,
                "ttlDays": args.ttl_days,
            }
            if args.guild:
                body["guild"] = args.guild
            status, res = api_with_session(config, "POST", "/keys", body=body)
        elif args.action == "list":
            status, res = api_with_session(config, "GET", "/keys")
        elif args.action == "revoke":
            status, res = api_with_session(config, "DELETE", f"/keys/{args.id}")
        else:
            log("key", f"unknown action {args.action!r}", level="error")
            return 2
    except ApiDown as exc:
        log("key", str(exc), level="error")
        return 1
    if status >= 300:
        log("key", f"{status}: {res.get('error', {}).get('message', res)}", level="error")
        return 1
    data = res.get("data", res)
    if args.action == "create":
        # Printed raw and once: it exists nowhere else. `--out` rather than `> key.txt`,
        # because the log lines share stdout and a redirect would capture them too.
        plaintext = str(data.get("key", ""))
        print(plaintext)
        if args.out:
            out_path = Path(args.out)
            if not out_path.is_absolute():
                out_path = ROOT / out_path
            out_path.parent.mkdir(parents=True, exist_ok=True)
            out_path.write_text(plaintext + "\n", encoding="utf-8")
            out_path.chmod(0o600)
            log("key", f"wrote {out_path} (chmod 600)")
        log("key", f"id={data.get('id')} hint={data.get('hint')} scopes={','.join(data.get('scopes', []))}")
        log("key", data.get("warning", "store it now — this is the only time the full key exists"), level="warn")
    else:
        print(json.dumps(data, indent=2, sort_keys=True))
    return 0


def cmd_reset(config: Config, args: argparse.Namespace) -> int:
    """Delete the data directory. Refuses to be a reflex: the path is printed, and it
    needs two flags. The databases in it are somebody's moderation history."""
    target = config.data_dir
    if not target.exists():
        log("reset", f"nothing to do: {target} does not exist")
        return 0
    listing = sorted(p.name for p in target.iterdir())
    if not args.confirm:
        log("reset", f"would delete {target}: {', '.join(listing) or '(empty)'}", level="warn")
        log("reset", "re-run with --confirm. This destroys warns, tickets, audit history, sessions and the install key.", level="warn")
        return 4
    if not args.i_understand_this_deletes_data:
        log("reset", "--confirm alone is not enough for a directory containing a database: pass --i-understand-this-deletes-data too", level="error")
        return 4
    for name in listing:
        path = target / name
        try:
            if path.is_dir() and not path.is_symlink():
                shutil.rmtree(path)
            else:
                path.unlink()
            log("reset", f"removed {name}")
        except OSError as exc:
            log("reset", f"could not remove {name}: {exc}", level="error")
            return 1
    return 0


def cmd_status(config: Config, _args: argparse.Namespace) -> int:
    try:
        status, res = api_request(config, "GET", "/status")
    except ApiDown as exc:
        log("status", str(exc), level="error")
        return 1
    print(json.dumps(res.get("data", res), indent=2, sort_keys=True))
    return 0 if status == 200 else 1


# ------------------------------------------------------------------------- cli


def parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(
        prog="main.py",
        description="Aerona launcher: get a binary (prebuilt, or build it), then serve the dashboard and the Discord worker.",
        epilog="Configuration is AERONA_* in the environment or .env — see .env.example.",
    )
    ap.add_argument("--dev", action="store_true", help="debug profile, no release build required")
    ap.add_argument("--verbose", action="store_true", help="also log what the launcher decides, not just what the child says")
    # A panel's startup command is fixed text with no subcommand in it, so `main.py` with
    # no arguments has to mean `run` — and it has to mean it with these defaults, which
    # is why they are set here as well as on the subparser.
    ap.set_defaults(
        func=cmd_run,
        no_bot=False,
        no_build=False,
        force=False,
        once=False,
        allow_existing_port=False,
    )
    sub = ap.add_subparsers(dest="command")

    run = sub.add_parser("run", help="make sure there is a binary, then start it and keep it up")
    run.add_argument("--no-bot", action="store_true", help="dashboard only (also AERONA_BOT=off)")
    run.add_argument("--no-build", action="store_true", help="never compile: use only what is already here")
    run.add_argument("--force", action="store_true", help="rebuild even when a usable binary exists")
    run.add_argument("--once", action="store_true", help="start, never restart: let the outer supervisor own the backoff")
    run.add_argument("--allow-existing-port", action="store_true", help="start even if the port answers (you know something I do not)")
    run.set_defaults(func=cmd_run)

    build_p = sub.add_parser("build", help="install a toolchain and build, if no usable binary exists yet")
    build_p.add_argument("--debug", action="store_true", help="debug profile instead of release")
    build_p.add_argument("--force", action="store_true", help="build even if a usable binary is present")
    build_p.add_argument("--rust-only", action="store_true", help="only install the Rust toolchain, build nothing")
    build_p.set_defaults(func=cmd_build)

    check_p = sub.add_parser("check", aliases=["doctor"], help="validate config, binaries and database without binding a port")
    check_p.add_argument("--no-dashboard", action="store_true", help="do not warn about a missing dashboard build")
    check_p.add_argument("--detailed", action="store_true", help="also ask the binary its network questions (port, token)")
    check_p.set_defaults(func=cmd_check)

    sub.add_parser("healthcheck", help="ask the API whether everything is up; exit 1 if not").set_defaults(func=cmd_healthcheck)
    sub.add_parser("status", help="print the API's /status payload").set_defaults(func=cmd_status)

    key = sub.add_parser("key", help="API keys, through the API")
    key.add_argument("action", choices=["create", "list", "revoke"])
    key.add_argument("--label", default="")
    # `default=None`, not `default=["read"]`: with `action="append"` argparse appends *to
    # the default object*, so `--scope read --scope moderate` would have produced
    # ["read", "read", "moderate"] — a scope nobody asked for twice.
    key.add_argument("--scope", action="append", default=None, help="repeatable: read|moderate|config|tickets")
    key.add_argument(
        "--out",
        default="",
        help="also write the plaintext key to this file, mode 0600 (the log lines share stdout, so `>` is not enough)",
    )
    key.add_argument("--guild", default="", help="restrict the key to one server id")
    key.add_argument("--ttl-days", type=int, default=90)
    key.add_argument("--id", type=int, default=0, help="key id, for revoke")
    key.set_defaults(func=cmd_key)

    reset = sub.add_parser("reset", help="delete the data directory (needs two flags)")
    reset.add_argument("--confirm", action="store_true")
    reset.add_argument("--i-understand-this-deletes-data", action="store_true")
    reset.set_defaults(func=cmd_reset)
    return ap


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    env = load_env()
    if args.command is None:
        # Not an error, and not a question either: on a panel this is the only
        # invocation that ever happens.
        log("main", "no command given: building if needed, then serving (see --help)")
    config = Config(env, dev=args.dev)
    try:
        return int(args.func(config, args))
    except KeyboardInterrupt:
        log("main", "interrupted")
        return 130


if __name__ == "__main__":
    sys.exit(main())
