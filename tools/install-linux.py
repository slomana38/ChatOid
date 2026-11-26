#!/usr/bin/env python3
"""Install the ChatOid Linux binaries and per-user desktop launchers."""

import os
import shutil
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent
APPLICATIONS = (
    ("chatoid", "ChatOid", "Masaüstü sohbet istemcisi", "chatoid.svg"),
    ("chatoid-agent", "ChatOid Ajan", "ChatOid yerel ajan istemcisi", "chatoid-agent.svg"),
)


def xdg_data_home(home: Path) -> Path:
    value = os.environ.get("XDG_DATA_HOME", "")
    candidate = Path(value) if value else None
    return candidate if candidate and candidate.is_absolute() else home / ".local" / "share"


def xdg_bin_home(home: Path) -> Path:
    # XDG_BIN_HOME is an optional convention; the portable fallback is ~/.local/bin.
    value = os.environ.get("XDG_BIN_HOME", "")
    candidate = Path(value) if value else None
    return candidate if candidate and candidate.is_absolute() else home / ".local" / "bin"


def desktop_exec_argument(argument: str) -> str:
    """Quote one Exec argument per Desktop Entry spec, including string escapes."""
    escaped = []
    for char in argument:
        codepoint = ord(char)
        if codepoint < 0x20 or codepoint == 0x7F:
            raise ValueError("Installation paths cannot contain control characters")
        if char == "\\":
            # A literal backslash needs two string-level escapes, then two Exec-level escapes.
            escaped.append("\\\\\\\\")
        elif char in ('"', "`", "$"):
            # Exec quoting is itself inside a Desktop Entry string value.
            escaped.append("\\\\" + char)
        elif char == "%":
            escaped.append("%%")
        else:
            escaped.append(char)
    return '"' + "".join(escaped) + '"'


def desktop_exec_command(binary: Path) -> str:
    # Use /bin/sh as the desktop Exec executable so a binary path containing a literal
    # percent sign stays an argument; some launchers treat %% in the executable token
    # as a field code before they resolve the path.
    command = ("/bin/sh", "-c", 'exec "$1"', binary.name, str(binary))
    return " ".join(desktop_exec_argument(argument) for argument in command)


def desktop_entry(binary: Path, name: str, comment: str, icon: str) -> str:
    return (
        "[Desktop Entry]\n"
        "Type=Application\n"
        f"Name={name}\n"
        f"Comment={comment}\n"
        f"Exec={desktop_exec_command(binary)}\n"
        f"Icon={icon}\n"
        "Terminal=false\n"
        "Categories=Network;InstantMessaging;\n"
    )


def atomic_copy(source: Path, destination: Path, mode: int) -> None:
    if not source.is_file():
        raise FileNotFoundError(f"Missing release file: {source.name}")
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="wb", dir=destination.parent, prefix=f".{destination.name}.", delete=False
        ) as staged:
            temporary = Path(staged.name)
            with source.open("rb") as original:
                shutil.copyfileobj(original, staged)
        temporary.chmod(mode)
        os.replace(temporary, destination)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def atomic_write(contents: str, destination: Path, mode: int) -> None:
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w", encoding="utf-8", dir=destination.parent,
            prefix=f".{destination.name}.", delete=False,
        ) as staged:
            temporary = Path(staged.name)
            staged.write(contents)
        temporary.chmod(mode)
        os.replace(temporary, destination)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def install() -> tuple[Path, Path]:
    home = Path.home()
    if not home.is_absolute():
        raise ValueError("HOME must be an absolute path")

    bin_home = xdg_bin_home(home)
    data_home = xdg_data_home(home)
    for binary, *_ in APPLICATIONS:
        desktop_exec_argument(str(bin_home / binary))
    applications_home = data_home / "applications"
    icons_home = data_home / "icons" / "hicolor" / "scalable" / "apps"
    bin_home.mkdir(parents=True, exist_ok=True)
    applications_home.mkdir(parents=True, exist_ok=True)
    icons_home.mkdir(parents=True, exist_ok=True)

    for binary, name, comment, icon in APPLICATIONS:
        installed_binary = bin_home / binary
        atomic_copy(ROOT / binary, installed_binary, 0o755)
        desktop_file = applications_home / f"{binary}.desktop"
        atomic_write(desktop_entry(installed_binary, name, comment, binary), desktop_file, 0o644)
        installed_icon = icons_home / icon
        atomic_copy(ROOT / "icons" / icon, installed_icon, 0o644)

    return bin_home, data_home


def main() -> int:
    try:
        bin_home, data_home = install()
    except (OSError, ValueError) as error:
        print(f"ChatOid installation failed: {error}", file=sys.stderr)
        return 1
    print(f"Installed ChatOid and ChatOid Ajan to {bin_home}")
    print(f"Desktop launchers and icons installed under {data_home}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
