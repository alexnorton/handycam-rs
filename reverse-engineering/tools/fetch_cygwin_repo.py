#!/usr/bin/env python3
"""Build a minimal, checksum-verified Cygwin Time Machine repository."""

from __future__ import annotations

import argparse
import hashlib
import lzma
import shutil
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
from pathlib import Path


DEFAULT_SITE = (
    "http://ctm.crouchingtigerhiddenfruitbat.org/"
    "pub/cygwin/circa/2016/08/30/104223"
)


@dataclass(frozen=True)
class Package:
    name: str
    categories: frozenset[str]
    requires: frozenset[str]
    path: str
    size: int
    sha512: str


def parse_setup(path: Path) -> dict[str, Package]:
    text = lzma.decompress(path.read_bytes()).decode("utf-8")
    packages: dict[str, Package] = {}
    for stanza in text.split("\n@ ")[1:]:
        lines = stanza.splitlines()
        name = lines[0].strip()
        current = lines[1:]
        for marker in ("[prev]", "[test]"):
            if marker in current:
                current = current[: current.index(marker)]

        fields: dict[str, str] = {}
        for line in current:
            if ": " in line and not line.startswith((" ", "\t")):
                key, value = line.split(": ", 1)
                fields.setdefault(key, value)
        if "install" not in fields:
            continue
        install = fields["install"].split()
        packages[name] = Package(
            name=name,
            categories=frozenset(fields.get("category", "").split()),
            requires=frozenset(fields.get("requires", "").split()),
            path=install[0],
            size=int(install[1]),
            sha512=install[2],
        )
    return packages


def dependency_closure(
    packages: dict[str, Package], requested: set[str]
) -> list[Package]:
    selected = {
        name for name, package in packages.items() if "Base" in package.categories
    }
    selected.update(requested)
    pending = list(selected)
    while pending:
        name = pending.pop()
        if name not in packages:
            raise KeyError(f"package metadata is missing required package {name!r}")
        for required in packages[name].requires:
            if required not in selected:
                selected.add(required)
                pending.append(required)
    return [packages[name] for name in sorted(selected)]


def valid_download(path: Path, package: Package) -> bool:
    if not path.is_file() or path.stat().st_size != package.size:
        return False
    digest = hashlib.sha512()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest() == package.sha512


def fetch(site: str, root: Path, package: Package) -> str:
    destination = root / package.path
    if valid_download(destination, package):
        return f"cached {package.name}"
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_suffix(destination.suffix + ".part")
    url = f"{site.rstrip('/')}/{package.path}"
    last_error: Exception | None = None
    for attempt in range(1, 4):
        try:
            request = urllib.request.Request(
                url, headers={"User-Agent": "handycam-cygwin-cache/1"}
            )
            with urllib.request.urlopen(request, timeout=60) as response:
                with temporary.open("wb") as output:
                    shutil.copyfileobj(response, output, length=1024 * 1024)
            temporary.replace(destination)
            if not valid_download(destination, package):
                destination.unlink(missing_ok=True)
                raise ValueError(f"checksum mismatch for {package.name}")
            return f"fetched {package.name}"
        except Exception as error:
            last_error = error
            temporary.unlink(missing_ok=True)
            if attempt < 3:
                time.sleep(attempt)
    raise RuntimeError(f"failed to fetch {package.name}: {last_error}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--setup", type=Path, default=Path("setup.xz"))
    parser.add_argument("--output", type=Path, default=Path("cygwin-repo"))
    parser.add_argument("--site", default=DEFAULT_SITE)
    parser.add_argument("--package", action="append", default=["openssh"])
    parser.add_argument("--jobs", type=int, default=8)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    packages = parse_setup(args.setup)
    selected = dependency_closure(packages, set(args.package))
    total = sum(package.size for package in selected)
    print(f"{len(selected)} packages, {total / (1024 * 1024):.1f} MiB")
    for package in selected:
        print(f"{package.name}\t{package.size / 1024:.1f} KiB")
    if args.dry_run:
        return 0

    metadata = args.output / "x86" / "setup.xz"
    metadata.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(args.setup, metadata)
    with ThreadPoolExecutor(max_workers=args.jobs) as executor:
        futures = {
            executor.submit(fetch, args.site, args.output, package): package
            for package in selected
        }
        for future in as_completed(futures):
            print(future.result(), flush=True)
    print("repository ready")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
