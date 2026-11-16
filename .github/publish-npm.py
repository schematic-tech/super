"""Prepare and publish the exact reviewed Super npm installer archive."""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import time
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import Request, urlopen


PACKAGE = "@schematic-tech/super"
REGISTRY = "https://registry.npmjs.org/"
FILES = {"package.json", "install.mjs", "cli.sh", "LICENSE", "README.md"}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def run(root, *args):
    return subprocess.check_output(args, cwd=root, text=True).strip()


def identity(root, version, commit):
    require(re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version),
            "Expected a three-part npm installer version.")
    require(re.fullmatch(r"[a-f0-9]{40}", commit), "Expected a full reviewed public commit ID.")
    require(run(root, "git", "rev-parse", "HEAD") == commit, "Public source commit mismatch.")
    require(not run(root, "git", "status", "--porcelain"), "Public source must be clean.")
    metadata = json.loads((root / "npm/package.json").read_text())
    require(metadata["name"] == PACKAGE and metadata["version"] == version, "npm package identity mismatch.")
    require(metadata["repository"] == {"type": "git", "url": "git+https://github.com/schematic-tech/super.git",
                                      "directory": "npm"}, "npm package must identify its public source.")
    require(metadata["publishConfig"] == {"access": "public", "registry": REGISTRY}, "Unexpected npm registry.")
    require(metadata["bin"] == {"super-install": "install.mjs"}, "The npm installer must not shadow the native CLI.")
    require(metadata["license"] == "Apache-2.0" and
            (root / "npm/LICENSE").read_bytes() == (root / "LICENSE").read_bytes(), "npm license mismatch.")
    require(set(metadata.get("scripts", {})) <= {"test"}, "Unexpected npm lifecycle scripts.")
    require(not any(metadata.get(key) for key in
                    ("dependencies", "devDependencies", "optionalDependencies", "peerDependencies", "bundledDependencies")),
            "Review npm dependencies before publication.")


def archive(root, version):
    path = root / f"target/npm/schematic-tech-super-{version}.tgz"
    with tarfile.open(path, "r:gz") as package:
        members = package.getmembers()
        require(len(members) == len(FILES) and {item.name for item in members} ==
                {f"package/{name}" for name in FILES}, "Unexpected files in npm archive.")
        for item in members:
            require(item.isfile(), "Only regular files may be packaged.")
            source = root / "npm" / item.name.removeprefix("package/")
            require(not source.is_symlink() and package.extractfile(item).read() == source.read_bytes(),
                    f"npm archive differs from reviewed source: {item.name}")
            if item.name == "package/install.mjs":
                require(item.mode & 0o111, "npm entry point must be executable.")
    integrity = "sha512-" + base64.b64encode(hashlib.sha512(path.read_bytes()).digest()).decode()
    return path, integrity


def prepare(root, version, commit, expected=None):
    identity(root, version, commit)
    output = root / "target/npm"
    output.mkdir(parents=True, exist_ok=True)
    result = json.loads(run(root / "npm", "npm", "pack", "--json", "--ignore-scripts",
                            "--pack-destination", str(output)))
    path, integrity = archive(root, version)
    require(len(result) == 1 and result[0]["filename"] == path.name and result[0]["integrity"] == integrity,
            "npm pack reported a different archive.")
    require(expected is None or integrity == expected, "npm archive integrity differs from the reviewed package.")
    return {"package": PACKAGE, "version": version, "commit": commit, "archive": str(path), "integrity": integrity}


def registry_integrity(version):
    request = Request(f"{REGISTRY}{quote(PACKAGE, safe='')}/{version}",
                      headers={"User-Agent": "Super-npm-release", "Cache-Control": "no-cache"})
    try:
        with urlopen(request, timeout=30) as response:
            metadata = json.load(response)
        require(metadata["name"] == PACKAGE and metadata["version"] == version, "Registry package identity mismatch.")
        require(not metadata.get("deprecated"), "Refusing to reuse a deprecated npm package.")
        integrity = metadata["dist"]["integrity"]
        require(isinstance(integrity, str) and integrity.startswith("sha512-"), "Missing npm registry integrity.")
        return integrity
    except HTTPError as error:
        if error.code != 404:
            raise
    return None


def publish(root, version, commit, expected):
    require(expected, "Publishing requires the reviewed npm archive integrity.")
    identity(root, version, commit)
    path, integrity = archive(root, version)
    require(integrity == expected, "npm archive integrity differs from the reviewed package.")
    observed = registry_integrity(version)
    require(observed is None or observed == integrity, "npm version already exists with different bytes.")
    if observed is not None:
        print(f"Already published: {PACKAGE} {version} (integrity verified)")
        return
    arguments = ["npm", "publish", str(path), "--access", "public", "--ignore-scripts", "--registry", REGISTRY]
    if os.environ.get("GITHUB_ACTIONS") == "true":
        arguments.append("--provenance")
    subprocess.run(arguments, cwd=root / "npm", check=True)
    deadline = time.monotonic() + 180
    while True:
        observed = registry_integrity(version)
        require(observed is None or observed == integrity, "Published npm archive integrity mismatch.")
        if observed is not None:
            return
        require(time.monotonic() < deadline, "Timed out confirming npm publication; retry this exact release.")
        time.sleep(5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("prepare", "publish"))
    parser.add_argument("--version", default=os.environ.get("RELEASE_NPM_VERSION"), required="RELEASE_NPM_VERSION" not in os.environ)
    parser.add_argument("--commit", default=os.environ.get("RELEASE_COMMIT"), required="RELEASE_COMMIT" not in os.environ)
    parser.add_argument("--integrity", default=os.environ.get("RELEASE_NPM_INTEGRITY"))
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    if args.mode == "prepare":
        print(json.dumps(prepare(root, args.version, args.commit, args.integrity), indent=2))
    else:
        publish(root, args.version, args.commit, args.integrity)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(str(error)) from None
