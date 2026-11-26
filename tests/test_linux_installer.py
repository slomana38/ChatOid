"""Smoke tests for the Linux archive installer and desktop entry generation."""

import importlib.util
import os
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
INSTALLER_PATH = ROOT / "tools" / "install-linux.py"
SPEC = importlib.util.spec_from_file_location("chatoid_linux_installer", INSTALLER_PATH)
INSTALLER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(INSTALLER)


def fixture_binary(name: str) -> bytes:
    path = shutil.which(name)
    if path is None:
        raise AssertionError(f"Missing Linux fixture binary: {name}")
    return Path(path).read_bytes()


def extract_archive(archive: Path, destination: Path) -> None:
    with tarfile.open(archive) as tar:
        for item in tar.getmembers():
            if not item.isfile():
                raise AssertionError(f"Unexpected archive entry: {item.name}")
            target = destination / item.name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(tar.extractfile(item).read())
            target.chmod(item.mode)


@unittest.skipUnless(
    sys.platform == "linux"
    and platform.machine().lower() in ("x86_64", "amd64")
    and shutil.which("sleep")
    and shutil.which("true"),
    "Linux x86_64 installer smoke test",
)
class LinuxInstallerTests(unittest.TestCase):
    def test_exec_argument_escapes_desktop_syntax(self):
        self.assertEqual(INSTALLER.desktop_exec_argument('a"b'), '"a\\\\"b"')
        self.assertEqual(INSTALLER.desktop_exec_argument("a`b"), '"a\\\\`b"')
        self.assertEqual(INSTALLER.desktop_exec_argument("a$b"), '"a\\\\$b"')
        self.assertEqual(INSTALLER.desktop_exec_argument("a%b"), '"a%%b"')
        self.assertEqual(INSTALLER.desktop_exec_argument("a\\b"), '"a\\\\\\\\b"')
        self.assertEqual(INSTALLER.desktop_exec_argument("a'b"), '"a\'b"')
        with self.assertRaises(ValueError):
            INSTALLER.desktop_exec_argument("a\nb")

    def test_release_archive_installs_both_xdg_launchers(self):
        with tempfile.TemporaryDirectory(prefix="chatoid install test ") as temp:
            temp = Path(temp)
            binary_dir = temp / "built binaries"
            binary_dir.mkdir()
            app_binary = fixture_binary("sleep")
            agent_binary = fixture_binary("true")
            (binary_dir / "chatoid").write_bytes(app_binary)
            (binary_dir / "chatoid-agent").write_bytes(agent_binary)

            output = temp / "dist"
            subprocess.run(
                [
                    sys.executable,
                    str(ROOT / "tools" / "package.py"),
                    "linux",
                    "--out",
                    str(output),
                    "--binaries",
                    str(binary_dir),
                ],
                cwd=ROOT,
                check=True,
            )
            archive = next(output.glob("*-linux-x64.tar.gz"))
            extracted = temp / "release with spaces"
            extracted.mkdir()
            extract_archive(archive, extracted)
            release_root = extracted / archive.name.removesuffix(".tar.gz")

            home = temp / "user home"
            bin_home = temp / 'bin "quoted" $cash `ticks` 100% \\slash'
            data_home = temp / 'data "quoted" $cash `ticks` 100% \\slash'
            env = os.environ.copy()
            env.update(
                HOME=str(home),
                XDG_BIN_HOME=str(bin_home),
                XDG_DATA_HOME=str(data_home),
            )
            subprocess.run(
                [str(release_root / "install-linux.py")],
                cwd=release_root,
                env=env,
                check=True,
                capture_output=True,
                text=True,
            )
            self.assertEqual(
                (release_root / "install-linux.py").stat().st_mode & 0o111,
                0o111,
            )

            applications = data_home / "applications"
            icons = data_home / "icons" / "hicolor" / "scalable" / "apps"
            validator = shutil.which("desktop-file-validate")
            gio = shutil.which("gio")
            for binary, title, icon in (
                ("chatoid", "ChatOid", "chatoid.svg"),
                ("chatoid-agent", "ChatOid Ajan", "chatoid-agent.svg"),
            ):
                installed_binary = bin_home / binary
                self.assertEqual(installed_binary.read_bytes(), app_binary if binary == "chatoid" else agent_binary)
                self.assertEqual(installed_binary.stat().st_mode & 0o777, 0o755)
                desktop = applications / f"{binary}.desktop"
                contents = desktop.read_text(encoding="utf-8")
                self.assertIn(f"Name={title}\n", contents)
                self.assertIn(
                    f"Exec={INSTALLER.desktop_exec_command(installed_binary)}\n",
                    contents,
                )
                self.assertIn(f"Icon={binary}\n", contents)
                self.assertEqual((icons / icon).read_bytes(), (release_root / "icons" / icon).read_bytes())
                if validator:
                    subprocess.run([validator, str(desktop)], check=True)

            if gio:
                subprocess.run(
                    [gio, "launch", str(applications / "chatoid-agent.desktop")],
                    env=env,
                    check=True,
                    capture_output=True,
                    text=True,
                )

            running_app = subprocess.Popen([str(bin_home / "chatoid"), "60"], env=env)
            try:
                time.sleep(0.05)
                self.assertIsNone(running_app.poll(), "fixture application exited before reinstall")
                subprocess.run(
                    [str(release_root / "install-linux.py")],
                    cwd=release_root,
                    env=env,
                    check=True,
                    capture_output=True,
                    text=True,
                )
                self.assertIsNone(running_app.poll(), "atomic reinstall stopped the running application")
            finally:
                running_app.terminate()
                running_app.wait(timeout=5)

    def test_relative_xdg_directories_use_user_defaults(self):
        with tempfile.TemporaryDirectory(prefix="chatoid xdg test ") as temp:
            home = Path(temp) / "home"
            with patch.dict(
                os.environ,
                {"XDG_BIN_HOME": "relative/bin", "XDG_DATA_HOME": "relative/data"},
                clear=False,
            ):
                self.assertEqual(INSTALLER.xdg_bin_home(home), home / ".local" / "bin")
                self.assertEqual(INSTALLER.xdg_data_home(home), home / ".local" / "share")


if __name__ == "__main__":
    unittest.main()
