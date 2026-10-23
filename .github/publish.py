"""Verify and publish the exact reviewed Super crate archives."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import time
import tomllib
from urllib.error import HTTPError
from urllib.request import Request, urlopen


CRATES = ("super-types", "super-tool")
REPOSITORY = "https://github.com/schematic-tech/super"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def run(root, *args):
    return subprocess.check_output(args, cwd=root, text=True).strip()


def expected_checksums(value):
    checksums = json.loads(value)
    require(isinstance(checksums, dict) and set(checksums) == set(CRATES)
            and all(isinstance(value, str) and re.fullmatch(r"[a-f0-9]{64}", value)
                    for value in checksums.values()), "Expected SHA-256 checksums for both release crates.")
    return checksums


def identity(root, version, commit):
    require(re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version),
            "Expected a three-part release version.")
    require(re.fullmatch(r"[a-f0-9]{40}", commit), "Expected a full public commit ID.")
    require(run(root, "git", "rev-parse", "HEAD") == commit, "Public source commit mismatch.")
    require(not run(root, "git", "status", "--porcelain"), "Public source must be clean.")
    workspace = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]
    require(workspace["package"]["version"] == version, "Workspace version mismatch.")
    require(workspace["package"]["repository"] == REPOSITORY, "Unexpected package repository.")
    require(set(workspace["members"]) == {f"crates/{name}" for name in CRATES}, "Unexpected workspace members.")


def archives(root, version, commit, checksums):
    for name in CRATES:
        path = root / f"target/package/{name}-{version}.crate"
        require(hashlib.sha256(path.read_bytes()).hexdigest() == checksums[name],
                f"Prepared archive checksum mismatch for {name}.")
        with tarfile.open(path, "r:gz") as archive:
            prefix = f"{name}-{version}"
            provenance = json.load(archive.extractfile(f"{prefix}/.cargo_vcs_info.json"))["git"]
            require(provenance["sha1"] == commit and not provenance.get("dirty", False),
                    f"Archive provenance mismatch for {name}.")
            metadata = tomllib.loads(archive.extractfile(f"{prefix}/Cargo.toml").read().decode())["package"]
            require(metadata["name"] == name and metadata["version"] == version
                    and metadata["repository"] == REPOSITORY, f"Archive identity mismatch for {name}.")


def registry_checksum(name, version):
    url = f"https://index.crates.io/{name[:2]}/{name[2:4]}/{name}"
    request = Request(url, headers={"User-Agent": "Super-release (https://github.com/schematic-tech/super)",
                                    "Cache-Control": "no-cache"})
    try:
        with urlopen(request, timeout=30) as response:
            for row in response:
                entry = json.loads(row)
                if entry["vers"] == version:
                    require(not entry["yanked"], f"{name} {version} is yanked; refusing to reuse it.")
                    return entry["cksum"]
    except HTTPError as error:
        if error.code != 404:
            raise
    return None


def publish(root, version, commit, checksums):
    identity(root, version, commit)
    archives(root, version, commit, checksums)
    # Check every package before the first upload, including on partial-release retries.
    observed = {name: registry_checksum(name, version) for name in CRATES}
    for name, checksum in observed.items():
        require(checksum is None or checksum == checksums[name],
                f"Registry checksum mismatch for {name}; refusing to publish.")
    for name in CRATES:
        if observed[name] is not None:
            print(f"Already published: {name} {version} (checksum verified)", flush=True)
            continue
        subprocess.run(["cargo", "publish", "--registry", "crates-io", "--locked", "--no-verify", "-p", name],
                       cwd=root, check=True)
        # Cargo repackages during publish; detect any change from the reviewed archives.
        archives(root, version, commit, checksums)
        deadline = time.monotonic() + 180
        while True:
            checksum = registry_checksum(name, version)
            require(checksum is None or checksum == checksums[name], f"Registry checksum mismatch for {name}.")
            if checksum is not None:
                break
            require(time.monotonic() < deadline, f"Timed out waiting for {name} {version}; rerun this exact release.")
            time.sleep(5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("prepare", "publish"))
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    version, commit = os.environ["RELEASE_VERSION"], os.environ["RELEASE_COMMIT"]
    checksums = expected_checksums(os.environ["RELEASE_CHECKSUMS"])
    identity(root, version, commit)
    if args.mode == "prepare":
        subprocess.run(["cargo", "package", "--workspace", "--locked", "--no-verify"], cwd=root, check=True)
        archives(root, version, commit, checksums)
    else:
        publish(root, version, commit, checksums)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, ValueError, OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(str(error)) from None
