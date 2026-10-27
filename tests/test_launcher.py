"""Tests for `main.py`, the launcher.

Stdlib `unittest` on purpose: the launcher exists for boxes where nothing is installed
yet, and a test suite that needs `pip install` to verify that box's bootstrap script
proves nothing. Run it with

    python3 -m unittest discover -s tests -t .

The pattern throughout is the same: build a throwaway install directory, point
`launcher.ROOT` at it, and assert on what the launcher decides - never on whether cargo
or npm work, which is those tools' business and is covered by `main.py check` on a real
machine.
"""

from __future__ import annotations

import importlib.util
import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]


def _load_launcher():
    spec = importlib.util.spec_from_file_location("aerona_launcher", REPO / "main.py")
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


launcher = _load_launcher()


class LauncherCase(unittest.TestCase):
    """A temp install directory, an isolated environment, and a launcher pointed at it."""

    def setUp(self) -> None:
        self._env = dict(os.environ)
        for key in [k for k in os.environ if k.startswith("AERONA_") or k == "PORT"]:
            del os.environ[key]
        self._root = tempfile.TemporaryDirectory(prefix="aerona-launcher-")
        self.addCleanup(self._root.cleanup)
        self.root = Path(self._root.name)
        launcher.ROOT = self.root
        launcher._PROBED.clear()

    def tearDown(self) -> None:
        os.environ.clear()
        os.environ.update(self._env)

    def write_fake_binary(self, name: str = "aerona", *, body: str | None = None, mode: int = 0o755) -> Path:
        """A stand-in for the real binary: it answers `--version` (which is all the
        launcher probes) and otherwise says nothing."""
        path = self.root / "bin" / name
        path.parent.mkdir(parents=True, exist_ok=True)
        text = (
            body
            or """#!/bin/sh
case "$1" in
  --version) echo "aerona 1.0.0 (api v1, worker protocol v1)" ;;
  check) echo "port            ${AERONA_PORT}" ;;
  *) : ;;
esac
exit 0
"""
        )
        path.write_text(text)
        path.chmod(mode)
        return path


class TestPlatform(LauncherCase):
    def test_platform_tag_is_os_and_arch(self):
        tag = launcher.platform_tag()
        self.assertRegex(tag, r"^[a-z0-9_]+-[a-z0-9_]+$")
        if sys.platform.startswith("linux") and platform_machine() in ("x86_64", "amd64"):
            self.assertEqual(tag, "linux-x86_64")

    def test_rustup_triple_is_a_real_asset_name_or_none(self):
        triple = launcher.rustup_triple()
        if triple is not None:
            self.assertGreaterEqual(triple.count("-"), 3, triple)
            self.assertTrue(
                any(part in triple for part in ("linux", "apple", "windows")),
                triple,
            )

    def test_binary_names_prefer_the_platform_and_always_include_the_bare_name(self):
        names = launcher.binary_names("aerona")
        self.assertEqual(names[0], f"aerona-{launcher.platform_tag()}{launcher.EXE}")
        self.assertIn("aerona", names)
        self.assertEqual(len(names), len(set(names)), "no duplicates")

    def test_candidate_paths_cover_prebuilt_and_target_and_path(self):
        paths = launcher.candidate_paths("aerona")
        rendered = [str(p) for p in paths]
        self.assertTrue(any("/bin/aerona" in p for p in rendered), rendered)
        self.assertTrue(any("/target/release/aerona" in p for p in rendered), rendered)
        self.assertTrue(all(p.endswith(launcher.EXE) or "/aerona" in p for p in rendered))
        # The prebuilt dir has to be searched before a build tree, or a shipped binary
        # loses to a stale local build.
        self.assertLess(
            rendered.index(str(self.root / "bin" / f"aerona-{launcher.platform_tag()}{launcher.EXE}")),
            rendered.index(str(self.root / "target" / "release" / f"aerona{launcher.EXE}")),
        )


class TestEnvFile(LauncherCase):
    def test_parse_env_file_handles_quotes_export_comments_and_case(self):
        path = self.root / ".env"
        path.write_text(
            "\n".join(
                [
                    "# a comment",
                    "",
                    "AERONA_PORT=8080",
                    "export AERONA_DATA_DIR=/srv/data",
                    'AERONA_BASE_URL="https://a.example"',
                    "AERONA_DB='data/x.db' # inline comment",
                    "lowercase_ignored=1",
                    "NO_EQUALS_SIGN",
                    "AERONA_TOKEN=  spaced  ",
                    "AFTER_QUOTE='data/x.db' # moved",
                    'AFTER_DOUBLE="a b" # note',
                    'UNTERMINATED="oops',
                ]
            )
        )
        got = launcher.parse_env_file(path)
        self.assertEqual(got["AERONA_PORT"], "8080")
        self.assertEqual(got["AERONA_DATA_DIR"], "/srv/data")
        self.assertEqual(got["AERONA_BASE_URL"], "https://a.example")
        self.assertEqual(got["AERONA_DB"], "data/x.db")
        self.assertEqual(got["AERONA_TOKEN"], "spaced")
        self.assertNotIn("lowercase_ignored", got)
        self.assertNotIn("NO_EQUALS_SIGN", got)
        # A comment after a closing quote is stripped, exactly as the Rust parser does
        # it - the launcher and the server read the same file and must not disagree.
        self.assertEqual(got["AFTER_QUOTE"], "data/x.db")
        self.assertEqual(got["AFTER_DOUBLE"], "a b")
        self.assertEqual(got["UNTERMINATED"], '"oops')

    def test_missing_file_is_empty_not_an_error(self):
        self.assertEqual(launcher.parse_env_file(self.root / "nope.env"), {})

    def test_redact_never_echoes_a_secret(self):
        os.environ["AERONA_TOKEN"] = "supersecrettoken.abcdef.1234567890"
        os.environ["AERONA_MASTER_KEY"] = "0123456789abcdef"
        line = launcher.redact(
            "AERONA_TOKEN=supersecrettoken.abcdef.1234567890 key aer_live_ABCDEFGHIJKLMNOPQRST"
        )
        self.assertNotIn("supersecrettoken", line)
        self.assertNotIn("ABCDEFGHIJKLMNOPQRST", line)
        self.assertIn("[redacted]", line)


class TestConfig(LauncherCase):
    def test_port_comes_from_aerona_port_then_the_panels_port_then_8080(self):
        self.assertEqual(launcher.Config({"AERONA_PORT": "9000"}).port, 9000)
        self.assertEqual(launcher.Config({"PORT": "25565"}).port, 25565, "a panel allocates PORT")
        self.assertEqual(launcher.Config({"AERONA_PORT": "9000", "PORT": "25565"}).port, 9000)
        self.assertEqual(launcher.Config({}).port, 8080)
        self.assertEqual(launcher.Config({"AERONA_PORT": "not-a-port"}).port, -1)
        self.assertTrue(any("AERONA_PORT" in p for p in launcher.Config({"AERONA_PORT": "x"}).problems()))

    def test_a_token_is_required_only_for_a_worker(self):
        self.assertTrue(any("AERONA_TOKEN" in p for p in launcher.Config({}).problems()))
        self.assertFalse(any("AERONA_TOKEN" in p for p in launcher.Config({"AERONA_BOT": "off"}).problems()))
        for off in ("off", "OFF", "false", "0", "no"):
            cfg = launcher.Config({"AERONA_BOT": off})
            self.assertTrue(cfg.bot_off, off)
            self.assertTrue(cfg.notes()[0].startswith("AERONA_BOT=off"))
        self.assertFalse(launcher.Config({"AERONA_BOT": "of"}).bot_off, "a typo is not 'off'")
        self.assertFalse(launcher.Config({"AERONA_BOT": "on"}).bot_off)

    def test_a_bad_token_is_still_a_bad_token_when_the_worker_is_off(self):
        problems = launcher.Config({"AERONA_BOT": "off", "AERONA_TOKEN": "https://discord.com/invite/x"}).problems()
        self.assertTrue(any("does not look like a bot token" in p for p in problems), problems)

    def test_privileged_ports_are_a_problem_unless_dev(self):
        cfg = launcher.Config({"AERONA_TOKEN": "x" * 40, "AERONA_PORT": "80"})
        self.assertTrue(any("needs root" in p for p in cfg.problems()))
        dev = launcher.Config({"AERONA_TOKEN": "x" * 40, "AERONA_PORT": "80"}, dev=True)
        self.assertFalse(any("needs root" in p for p in dev.problems()))

    def test_a_trailing_slash_is_fatal_and_plain_http_is_only_a_note(self):
        """A panel gives one raw TCP port, so http on a public name is the ordinary shape
        of an install. It belongs in `notes()`, where it can be read; keeping it in
        `problems()` meant `main.py run` refused to start on the one configuration the
        deploy guide tells you to write, and refused it for a reason that was not even
        true - the origin check passes *because* the value matches the address bar."""
        cfg = launcher.Config({"AERONA_TOKEN": "x" * 40, "AERONA_BASE_URL": "http://aerona.example.com/"})
        problems = cfg.problems()
        self.assertTrue(any("must not end with a slash" in x for x in problems), problems)
        self.assertFalse(any("plain http" in x for x in problems), problems)
        notes = cfg.notes()
        self.assertTrue(any("plain http" in x and "session cookie" in x for x in notes), notes)
        self.assertTrue(any("ssh -L" in x for x in notes), notes)
        # An https origin is not a note at all.
        secure = launcher.Config({"AERONA_TOKEN": "x" * 40, "AERONA_BASE_URL": "https://bot.example.com"})
        self.assertFalse(any("plain http" in x for x in secure.notes()), secure.notes())

    def test_the_redirect_uri_is_said_before_the_first_login(self):
        """Discord answers `Invalid OAuth2 redirect_uri` in the user's browser, from a
        request that never reaches this install - there is no log line of ours that
        explains it. So the launcher prints the exact string it will send, and the missing
        half of the credentials gets its own sentence instead of a silent half-configured
        login button."""
        pinned = launcher.Config(
            {
                "AERONA_TOKEN": "x" * 40,
                "AERONA_DISCORD_CLIENT_ID": "1234",
                "AERONA_DISCORD_CLIENT_SECRET": "secret",
                "AERONA_OAUTH_REDIRECT_URI": "http://127.0.0.1:25154/api/v1/auth/discord/callback",
            }
        )
        note = [n for n in pinned.notes() if "redirect_uri=" in n]
        self.assertEqual(len(note), 1, pinned.notes())
        self.assertIn("http://127.0.0.1:25154/api/v1/auth/discord/callback", note[0])
        self.assertIn("Save Changes", note[0])
        self.assertIn("character for character", note[0])

        derived = launcher.Config(
            {
                "AERONA_TOKEN": "x" * 40,
                "AERONA_DISCORD_CLIENT_ID": "1234",
                "AERONA_DISCORD_CLIENT_SECRET": "secret",
                "AERONA_BASE_URL": "http://panel.example:25154",
            }
        )
        text = " ".join(derived.notes())
        self.assertIn("http://panel.example:25154" + launcher.DISCORD_CALLBACK_PATH, text)
        self.assertIn("one entry per address", text)

        half = launcher.Config({"AERONA_TOKEN": "x" * 40, "AERONA_DISCORD_CLIENT_ID": "1234"})
        self.assertTrue(any("CLIENT_SECRET is not" in n for n in half.notes()), half.notes())
        quiet = launcher.Config({"AERONA_TOKEN": "x" * 40})
        self.assertFalse(any("redirect_uri" in n for n in quiet.notes()), quiet.notes())

    def test_an_unset_base_url_stays_unset_and_the_launcher_uses_its_own_origin(self):
        """The launcher must not invent `AERONA_BASE_URL` it only means for itself: a
        written-down localhost origin switches the API's same-host fallback off, and every
        mutation from the panel's IP then 403s."""
        cfg = launcher.Config({"AERONA_PORT": "27801"})
        self.assertEqual(cfg.base_url, "")
        self.assertEqual(cfg.origin, "http://127.0.0.1:27801")
        self.assertFalse(any("plain http" in x for x in cfg.problems()), cfg.problems())
        self.assertFalse(any("plain http" in x for x in cfg.notes()), cfg.notes())
        named = launcher.Config({"AERONA_BASE_URL": "https://aerona.example.com"})
        self.assertEqual(named.base_url, "https://aerona.example.com")
        self.assertEqual(named.origin, "https://aerona.example.com")

    def test_paths_are_resolved_against_the_project_root(self):
        cfg = launcher.Config({"AERONA_DATA_DIR": "data", "AERONA_DB": "", "AERONA_STATIC_DIR": "dashboard/dist"})
        self.assertEqual(cfg.data_dir, self.root / "data")
        self.assertEqual(cfg.db, self.root / "data" / "aerona.db")
        self.assertEqual(cfg.static_dir, self.root / "dashboard" / "dist")
        absolute = launcher.Config({"AERONA_DATA_DIR": "/srv/aerona-data"})
        self.assertEqual(absolute.data_dir, Path("/srv/aerona-data"))

    @unittest.skipIf(hasattr(os, "geteuid") and os.geteuid() == 0, "root ignores directory modes")
    def test_an_unwritable_data_dir_is_a_problem(self):
        blocked = self.root / "locked"
        blocked.mkdir()
        blocked.chmod(0o500)
        try:
            problems = launcher.Config({"AERONA_DATA_DIR": str(blocked), "AERONA_TOKEN": "x" * 40}).problems()
            self.assertTrue(any("not writable" in p for p in problems), problems)
        finally:
            blocked.chmod(0o755)

    def test_child_env_pins_the_paths_both_halves_share(self):
        cfg = launcher.Config({"AERONA_BOT": "off", "AERONA_PORT": "9123"})
        env = cfg.child_env("aerona")
        self.assertEqual(env["AERONA_PORT"], "9123")
        self.assertEqual(env["AERONA_BOT"], "off")
        self.assertEqual(env["AERONA_DB"], str(cfg.db))
        self.assertEqual(env["AERONA_STATIC_DIR"], str(cfg.static_dir))
        self.assertEqual(env["AERONA_COMPONENT"], "aerona")


class TestBinaryDiscovery(LauncherCase):
    @unittest.skipIf(os.name == "nt", "the fake binaries here are /bin/sh scripts")
    def test_usable_fixes_a_lost_execute_bit(self):
        path = self.write_fake_binary("aerona", mode=0o644)
        self.assertFalse(os.access(path, os.X_OK))
        self.assertTrue(launcher.usable(path), "a zip that dropped the exec bit is the common case")
        self.assertTrue(os.access(path, os.X_OK))
        self.assertTrue(stat.S_IMODE(path.stat().st_mode) & 0o100)

    @unittest.skipIf(os.name == "nt", "the fake binaries here are /bin/sh scripts")
    def test_usable_rejects_a_binary_that_cannot_run_here(self):
        wrong = self.write_fake_binary("aerona", body="#!/bin/sh\nexit 126\n")
        self.assertFalse(launcher.usable(wrong))
        garbage = self.root / "bin" / "aerona"
        garbage.write_bytes(b"\x7fELF not really an elf")
        garbage.chmod(0o755)
        self.assertFalse(launcher.usable(garbage))
        self.assertFalse(launcher.usable(self.root / "bin" / "does-not-exist"))

    @unittest.skipIf(os.name == "nt", "the fake binaries here are /bin/sh scripts")
    def test_find_unified_prefers_the_platform_specific_name(self):
        generic = self.write_fake_binary("aerona")
        tagged = self.write_fake_binary(f"aerona-{launcher.platform_tag()}")
        self.assertEqual(launcher.find_unified(), tagged)
        tagged.unlink()
        self.assertEqual(launcher.find_unified(), generic)
        generic.unlink()
        self.assertIsNone(launcher.find_unified())

    @unittest.skipIf(os.name == "nt", "the fake binaries here are /bin/sh scripts")
    def test_legacy_pair_needs_both_binaries(self):
        self.write_fake_binary("aerona-api")
        self.assertIsNone(launcher.find_legacy(), "half a pair would come up with no worker")
        self.write_fake_binary("aerona-bot")
        pair = launcher.find_legacy()
        self.assertIsNotNone(pair)
        self.assertEqual([p.name for p in pair], ["aerona-api", "aerona-bot"])

    @unittest.skipIf(os.name == "nt", "the fake binaries here are /bin/sh scripts")
    def test_ensure_runtime_uses_a_prebuilt_binary_and_never_builds(self):
        cfg = launcher.Config({"AERONA_BOT": "off"})
        path = self.write_fake_binary("aerona")
        kind, argv = launcher.ensure_runtime(cfg)
        self.assertEqual((kind, argv), ("unified", [str(path)]))
        # The same answer with --no-build, which is the panel's fast path.
        self.assertEqual(launcher.ensure_runtime(cfg, allow_build=False)[0], "unified")

    def test_ensure_runtime_refuses_to_build_when_told_not_to(self):
        kind, argv = launcher.ensure_runtime(launcher.Config({"AERONA_BOT": "off"}), allow_build=False)
        self.assertEqual((kind, argv), ("none", []))

    @unittest.skipIf(os.name == "nt", "the fake binaries here are /bin/sh scripts")
    def test_autoinstall_can_be_turned_off_and_says_what_to_do_instead(self):
        os.environ["AERONA_NO_AUTOINSTALL"] = "1"
        cargo = launcher.install_rust()
        self.assertIsNone(cargo, "with the switch on, no download may start")
        self.assertFalse((self.root / ".cache").exists(), "and no toolchain directory is created")


class TestBuildParallelism(LauncherCase):
    """`-j` from RAM, because the failure it prevents is a killed compile, not a slow one."""

    def test_a_small_box_builds_with_one_job(self):
        self.assertEqual(launcher.build_jobs(2 * 1024 * 1024), "1")
        self.assertEqual(launcher.build_jobs(int(1.9 * 1024 * 1024)), "1")

    def test_a_medium_box_gets_two_and_a_workstation_gets_no_opinion(self):
        self.assertEqual(launcher.build_jobs(6 * 1024 * 1024), "2")
        self.assertIsNone(launcher.build_jobs(32 * 1024 * 1024), "no cap where there is room")

    def test_an_unknown_machine_gets_no_flag_rather_than_a_guess(self):
        self.assertIsNone(launcher.build_jobs(0))

    def test_the_operator_can_override_in_both_directions(self):
        os.environ["AERONA_BUILD_JOBS"] = "8"
        self.assertEqual(launcher.build_jobs(2 * 1024 * 1024), "8", "an override beats the heuristic")
        os.environ["AERONA_BUILD_JOBS"] = "auto"
        self.assertIsNone(launcher.build_jobs(2 * 1024 * 1024), "auto hands the decision to cargo")


class TestToolchainPlacement(LauncherCase):
    def test_cache_dir_defaults_inside_the_project_and_can_be_moved(self):
        self.assertEqual(launcher.cache_dir(), self.root / ".cache")
        os.environ["AERONA_TOOLCHAIN_DIR"] = "/srv/aerona-tools"
        self.assertEqual(launcher.cache_dir(), Path("/srv/aerona-tools"))

    def test_toolchain_env_puts_the_local_cargo_first_on_path(self):
        bindir = launcher.cache_dir() / "cargo" / "bin"
        bindir.mkdir(parents=True)
        env = launcher.toolchain_env()
        self.assertTrue(env["PATH"].startswith(str(bindir)), env["PATH"][:120])
        self.assertEqual(env["RUSTUP_HOME"], str(launcher.cache_dir() / "rustup"))
        self.assertEqual(env["CARGO_HOME"], str(launcher.cache_dir() / "cargo"))
        # Never colour, because the log is written to a file and read by `grep`.
        self.assertEqual(env["CARGO_TERM_COLOR"], "never")

    def test_cargo_binary_finds_the_installed_toolchain(self):
        fake = launcher.cache_dir() / "cargo" / "bin" / f"cargo{launcher.EXE}"
        fake.parent.mkdir(parents=True)
        fake.write_text("#!/bin/sh\nexit 0\n")
        fake.chmod(0o755)
        self.assertEqual(launcher.cargo_binary(), fake)


class TestFirstBootEnv(LauncherCase):
    def test_it_writes_env_with_a_generated_master_key_once(self):
        (self.root / ".env.example").write_text(
            "# Aerona - required\n"
            "AERONA_TOKEN=put-your-discord-bot-token-here\n"
            "AERONA_PORT=8080\n"
            "AERONA_BASE_URL=http://localhost:8080\n"
            "AERONA_MASTER_KEY=\n"
            "AERONA_BOT=\n"
        )
        cfg = launcher.Config({"AERONA_PORT": "25565"})
        created = launcher.ensure_env_file(cfg)
        self.assertEqual(created, self.root / ".env")
        mode = stat.S_IMODE(created.stat().st_mode)
        self.assertEqual(mode, 0o600, "a file with a pepper in it is not group-readable")
        text = created.read_text()
        key = [line for line in text.splitlines() if line.startswith("AERONA_MASTER_KEY=")][0].split("=", 1)[1]
        self.assertEqual(len(key), 64, "32 bytes, hex")
        self.assertIn("AERONA_PORT=25565", text, "the panel's port has to be in the file it writes")
        self.assertIn("AERONA_TOKEN=put-your-discord-bot-token-here", text, "never invent a token")
        self.assertIn("AERONA_BASE_URL=\n", text, "a localhost placeholder must be blanked, not kept")
        self.assertNotIn("AERONA_BASE_URL=http", text, "never write an origin the operator did not name")
        self.assertIsNone(launcher.ensure_env_file(cfg), "an existing .env is never touched")

    def test_bot_off_comments_the_token_out_so_the_default_file_boots(self):
        (self.root / ".env.example").write_text("AERONA_TOKEN=put-your-discord-bot-token-here\nAERONA_PORT=8080\nAERONA_BOT=\n")
        cfg = launcher.Config({"AERONA_BOT": "off"})
        launcher.ensure_env_file(cfg)
        text = (self.root / ".env").read_text()
        self.assertIn("AERONA_BOT=off", text)
        self.assertIn("# AERONA_TOKEN=", text)
        self.assertNotIn("AERONA_TOKEN=put-your", text, "a placeholder that is not commented out is a boot failure")

    def test_without_a_template_it_still_writes_something_valid(self):
        cfg = launcher.Config({"AERONA_BOT": "off", "AERONA_PORT": "9099"})
        created = launcher.ensure_env_file(cfg)
        parsed = launcher.parse_env_file(created)
        self.assertEqual(parsed["AERONA_PORT"], "9099")
        self.assertEqual(parsed["AERONA_BOT"], "off")
        self.assertEqual(parsed["AERONA_TOKEN"], "")
        self.assertEqual(len(parsed["AERONA_MASTER_KEY"]), 64)

    def test_a_read_only_project_directory_is_a_warning_not_a_crash(self):
        self.root.chmod(0o500)
        try:
            self.assertIsNone(launcher.ensure_env_file(launcher.Config({"AERONA_BOT": "off"})))
        finally:
            self.root.chmod(0o755)


class TestDashboardGate(LauncherCase):
    def test_missing_index_html_is_what_needs_a_build(self):
        cfg = launcher.Config({})
        self.assertTrue(launcher.dashboard_needs_build(cfg))
        (self.root / "dashboard" / "dist").mkdir(parents=True)
        (self.root / "dashboard" / "dist" / "index.html").write_text("<html></html>")
        self.assertFalse(launcher.dashboard_needs_build(cfg))

    def test_the_htmls_asset_list_is_read_from_both_documents(self):
        """The launcher can answer "is the dashboard complete" because the documents say what they
        need. Both files matter: `shell.html` is what a deep link to a guild page loads, and it names
        the same stylesheet the entry document does."""
        dist = self.root / "dashboard" / "dist"
        (dist / "assets").mkdir(parents=True)
        (dist / "index.html").write_text(
            '<link rel="stylesheet" href="/assets/style-Ab12.css">'
            '<script type="module" src="/assets/index-Cd34.js"></script>',
            encoding="utf-8",
        )
        (dist / "shell.html").write_text(
            '<link rel="stylesheet" href="/assets/style-Ab12.css">'
            '<img src="/assets/logo-2.png">',
            encoding="utf-8",
        )
        (dist / "assets" / "style-Ab12.css").write_text("a {}", encoding="utf-8")
        self.assertEqual(
            launcher.referenced_assets(dist),
            ["assets/index-Cd34.js", "assets/logo-2.png", "assets/style-Ab12.css"],
        )

    def test_a_query_suffix_on_an_asset_url_is_not_part_of_the_name(self):
        dist = self.root / "dashboard" / "dist"
        (dist / "assets").mkdir(parents=True)
        (dist / "index.html").write_text(
            '<link rel="stylesheet" href="/assets/style-Ab12.css?v=2">', encoding="utf-8"
        )
        self.assertEqual(launcher.referenced_assets(dist), ["assets/style-Ab12.css"])

    def test_no_dashboard_directory_is_a_note_not_a_failure(self):
        """A dashboard nobody built must not stop the API from coming up: the skip path
        returns 0, because `run` treats a non-zero build as fatal."""
        self.assertFalse((self.root / "dashboard").exists())
        self.assertEqual(launcher.build_dashboard(launcher.Config({})), 0)


def platform_machine() -> str:
    import platform

    return (platform.machine() or "").lower()


if __name__ == "__main__":
    unittest.main()
