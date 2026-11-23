"""Create and verify ChatOid distribution archives from explicit file lists."""

import argparse
import hashlib
import struct
import tarfile
import tomllib
from pathlib import Path
from zipfile import ZIP_DEFLATED, ZipFile


ROOT = Path(__file__).resolve().parents[1]
VERSION = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
STEM = f"chatoid-{VERSION}"
NAMES = {
    "windows": f"{STEM}-windows-x64.zip",
    "linux": f"{STEM}-linux-x64.tar.gz",
    "source": f"{STEM}-source.zip",
}
BINARIES = {
    "windows": ("chatoid.exe", "chatoid-agent.exe"),
    "linux": ("chatoid", "chatoid-agent"),
}
DOCS = ("README.md", "LICENSE")


def checksum(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def source_files():
    return sorted(ROOT.glob("src/**/*.rs")) + sorted(ROOT.glob("tests/*.rs")) + [
        ROOT / name
        for name in (*DOCS, "Cargo.toml", "Cargo.lock", "build.rs", ".gitignore", ".gitattributes", "tools/package.py",
                     ".github/workflows/release.yml")
    ]


def check_binary(data, platform):
    if platform == "windows":
        offset = struct.unpack_from("<I", data, 0x3C)[0]
        if data[:2] == b"MZ" and data[offset:offset + 6] == b"PE\0\0\x64\x86":
            return
    elif data[:6] == b"\x7fELF\x02\x01" and data[18:20] == b"\x3e\0":
        return
    raise ValueError(f"Unexpected architecture for {platform}")


def package(platform, out, binaries):
    archive = out / NAMES[platform]
    archive_root = NAMES[platform].removesuffix(".zip").removesuffix(".tar.gz")
    if platform == "source":
        with ZipFile(archive, "w", ZIP_DEFLATED) as target:
            for file in source_files():
                target.write(file, f"{archive_root}/{file.relative_to(ROOT).as_posix()}")
        return
    for binary in BINARIES[platform]:
        check_binary((binaries / binary).read_bytes(), platform)
    if platform == "windows":
        with ZipFile(archive, "w", ZIP_DEFLATED) as target:
            for name in DOCS:
                target.write(ROOT / name, f"{archive_root}/{name}")
            for name in BINARIES[platform]:
                target.write(binaries / name, f"{archive_root}/{name}")
    else:
        with tarfile.open(archive, "w:gz") as target:
            for name in DOCS:
                target.add(ROOT / name, f"{archive_root}/{name}")
            for name in BINARIES[platform]:
                entry = target.gettarinfo(str(binaries / name), f"{archive_root}/{name}")
                entry.mode = 0o755
                with (binaries / name).open("rb") as file:
                    target.addfile(entry, file)


def inspect_archive(platform, path):
    prefix = path.name.removesuffix(".zip").removesuffix(".tar.gz")
    if platform == "source":
        with ZipFile(path) as archive:
            actual = sorted(archive.namelist())
            expected = sorted(f"{prefix}/{file.relative_to(ROOT).as_posix()}" for file in source_files())
            if actual != expected:
                raise ValueError("Unexpected source package contents")
            if any(archive.read(f"{prefix}/{file.relative_to(ROOT).as_posix()}") != file.read_bytes()
                   for file in source_files()):
                raise ValueError("Source package differs from the checkout")
        return
    expected = sorted(f"{prefix}/{name}" for name in (*DOCS, *BINARIES[platform]))
    if platform == "windows":
        with ZipFile(path) as archive:
            if sorted(archive.namelist()) != expected:
                raise ValueError("Unexpected Windows package contents")
            for name in BINARIES[platform]:
                check_binary(archive.read(f"{prefix}/{name}"), platform)
            for name in DOCS:
                if archive.read(f"{prefix}/{name}") != (ROOT / name).read_bytes():
                    raise ValueError(f"Unexpected Windows copy of {name}")
    else:
        with tarfile.open(path) as archive:
            if sorted(archive.getnames()) != expected or not all(item.isfile() for item in archive):
                raise ValueError("Unexpected Linux package contents")
            for name in BINARIES[platform]:
                entry = archive.getmember(f"{prefix}/{name}")
                if entry.mode & 0o111 == 0:
                    raise ValueError(f"{name} lacks execute permission")
                check_binary(archive.extractfile(entry).read(), platform)
            for name in DOCS:
                if archive.extractfile(f"{prefix}/{name}").read() != (ROOT / name).read_bytes():
                    raise ValueError(f"Unexpected Linux copy of {name}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=(*NAMES, "manifest", "verify"))
    parser.add_argument("--out", type=Path, default=ROOT / "dist")
    parser.add_argument("--binaries", type=Path, default=ROOT / "target" / "release")
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    manifest = args.out / f"{STEM}-sha256.txt"
    if args.action in NAMES:
        package(args.action, args.out, args.binaries)
        inspect_archive(args.action, args.out / NAMES[args.action])
    elif args.action == "manifest":
        lines = [f"{checksum(args.out / name)}  {name}" for name in NAMES.values() if (args.out / name).is_file()]
        if not lines:
            raise ValueError("No release archives found")
        manifest.write_text("\n".join(lines) + "\n", encoding="utf-8")
    else:
        found = set()
        for line in manifest.read_text(encoding="utf-8").splitlines():
            digest, name = line.split("  ", 1)
            if name in found or name not in NAMES.values() or checksum(args.out / name) != digest:
                raise ValueError(f"Invalid entry for {name}")
            platform = next(key for key, value in NAMES.items() if value == name)
            inspect_archive(platform, args.out / name)
            print(f"Verified {name}")
            found.add(name)
        if not found or found != {name for name in NAMES.values() if (args.out / name).is_file()}:
            raise ValueError("Checksum list does not match the available archives")


if __name__ == "__main__":
    main()
