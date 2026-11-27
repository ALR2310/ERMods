#!/usr/bin/env python3
"""Build a mod and publish it to Nexus Mods as a new version of its mod file.

    python scripts/nexus-deploy.py <crate> [--version X.Y.Z] [--file-id N] [--category optional] [--dry-run]

Reads the changelog of the version from mods/<crate>/CHANGELOG.md, runs scripts/build-mod.ps1, uploads
the zip, attaches it to the mod file, pushes the changelog and appends the deploy to
.local/manifest.json. Needs NEXUS_API_KEY in the environment or in .env, and an entry for the mod
in .local/manifest.json (id, internalId, crate, deploys).

Use `--archive PATH` to publish the existing GitHub Release asset using .github/nexus-mods.json,
or `--verify-only` to read back the released file, mod version and changelog from Nexus.
`--test-nexus` directs either mode to the unpublished or hidden test mod.

The zip goes to the main mod file of the manifest. `--category optional` (or `miscellaneous`) puts it
on another mod file instead (`--file-id` is then required): it is not the mod manager's default download,
does not change the mod's version and does not push a mod changelog.

Exit codes: 0 done, 1 error, 2 several candidate mod files (pass --file-id).
"""

import argparse
import base64
import hashlib
import json
import os
import re
import subprocess
import sys
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / ".local" / "manifest.json"
NEXUS_MODS = ROOT / ".github" / "nexus-mods.json"
NEXUS_TEST_MOD = {"id": 11735, "internalId": "18610093305303"}
# What the test mod may be: not listed at all, or hidden. A listed mod is never a test target.
NEXUS_TEST_STATUSES = ("not_published", "hidden")
API = "https://api.nexusmods.com"
USER_AGENT = "ermods-deploy"


class Fail(Exception):
    def __init__(self, message, code=1):
        super().__init__(message)
        self.code = code


def api_key():
    key = os.environ.get("NEXUS_API_KEY")
    env = ROOT / ".env"
    if not key and env.is_file():
        for line in env.read_text(encoding="utf-8").splitlines():
            name, _, value = line.partition("=")
            if name.strip() == "NEXUS_API_KEY":
                key = value.strip().strip("\"'")
    if not key:
        raise Fail("NEXUS_API_KEY is not set (environment or .env)")
    return key


def call(method, url, key, body=None, data=None, headers=None):
    request = urllib.request.Request(url, method=method, data=data)
    request.add_header("User-Agent", USER_AGENT)
    host = urllib.parse.urlsplit(url)
    if host.scheme == "https" and host.netloc == "api.nexusmods.com":
        request.add_unredirected_header("apikey", key)
    if body is not None:
        request.data = json.dumps(body).encode("utf-8")
        request.add_header("Content-Type", "application/json")
    for name, value in (headers or {}).items():
        request.add_header(name, value)
    try:
        with urllib.request.urlopen(request, timeout=300) as response:
            text = response.read().decode("utf-8")
    except urllib.error.HTTPError as error:
        text = error.read().decode("utf-8", "replace")
        try:
            detail = json.loads(text).get("detail") or text
        except ValueError:
            detail = text
        raise Fail(f"{method} {url.split('?')[0]} failed ({error.code}): {detail[:400]}") from None
    return json.loads(text) if text.strip() else {}


def cargo_info(crate):
    cargo = tomllib.loads((ROOT / "mods" / crate / "Cargo.toml").read_text(encoding="utf-8"))
    return cargo["package"]["version"], cargo["lib"]["name"]


def changelog_lines(crate, version):
    text = (ROOT / "mods" / crate / "CHANGELOG.md").read_text(encoding="utf-8")
    section = re.search(rf"^## \[{re.escape(version)}\][^\n]*\n(.*?)(?=^## |\Z)", text, re.S | re.M)
    if not section:
        raise Fail(f"CHANGELOG.md has no section [{version}]")
    lines = [line[2:].strip() for line in section.group(1).splitlines() if line.startswith("- ")]
    if not lines:
        raise Fail(f"the section [{version}] of CHANGELOG.md has no bullets")
    return lines


def git(*args):
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True, check=True).stdout


def pick_file(entry, files, wanted, main):
    """The mod file that gets the new version, and the name its versions carry."""
    active = [f for f in files if f.get("is_active")]
    known = entry.get("file") or {}
    chosen = None
    if wanted:
        chosen = next((f for f in files if str(f["id"]) == str(wanted)), None)
    elif main and known:
        chosen = next((f for f in files if str(f["id"]) == str(known["id"])), None)
    elif main and len(active) == 1:
        chosen = active[0]
    if chosen is None:
        listing = "\n".join(f"  {f['id']}  {f['name']}  (active={f.get('is_active')})" for f in files)
        raise Fail("several candidate mod files, choose one and pass --file-id:\n" + listing, 2)
    stored = known.get("name") if str(chosen["id"]) == str(known.get("id")) else None
    name = stored or re.sub(r"[- ]?v?\d+(\.\d+)+$", "", chosen["name"]).strip()
    return chosen, name


def build(crate, lib, version):
    result = subprocess.run(
        ["pwsh", "-NoProfile", "-File", str(ROOT / "scripts" / "build-mod.ps1"),
         "-Mod", lib, "-Zip", "-Version", version],
        cwd=ROOT,
    )
    if result.returncode != 0:
        raise Fail("build failed")
    zip_path = ROOT / "builds" / crate / f"{lib}-{version}.zip"
    if not zip_path.is_file():
        raise Fail(f"{zip_path} was not produced")
    return zip_path


def upload(key, zip_path, md5_hex, md5_b64):
    size = zip_path.stat().st_size
    session = call("POST", f"{API}/v3/uploads", key,
                   body={"size_bytes": size, "filename": zip_path.name, "md5": md5_hex})["data"]
    call("PUT", session["presigned_url"], key, data=zip_path.read_bytes(), headers={
        "Content-Disposition": f'attachment; filename="{zip_path.name}"',
        "Content-MD5": md5_b64,
        "Content-Type": "application/octet-stream",
    })
    call("POST", f"{API}/v3/uploads/{session['id']}/finalise", key)
    for _ in range(60):
        state = call("GET", f"{API}/v3/uploads/{session['id']}", key)["data"]["state"]
        if state == "available":
            return session["id"]
        time.sleep(3)
    raise Fail("the upload did not become available in 3 minutes")


def file_description(lines):
    changelog = "\n".join(lines)
    if len(changelog) <= 255:
        return changelog
    return ("See the changelog for details.\n\n"
            "Mod files may be detected as false positives by antivirus software.")


def attach_version(key, target_id, name, version, category, zip_path, lines, *, new_file=False):
    digest = hashlib.md5(zip_path.read_bytes()).digest()
    upload_id = upload(key, zip_path, digest.hex(), base64.b64encode(digest).decode())
    main_file = category == "main"
    body = {
        "upload_id": upload_id, "name": name, "version": version, "file_category": category,
        "update_mod_version": main_file, "primary_mod_manager_download": main_file,
        "description": file_description(lines),
    }
    url = f"{API}/v3/mod-files"
    if new_file:
        body["mod_id"] = target_id
    else:
        url = f"{url}/{target_id}/versions"
    call("POST", url, key, body=body)


def nexus_file_version(key, url, version):
    versions = call("GET", url, key)["data"]["versions"]
    matches = [item for item in versions if item["version"] == version]
    if len(matches) > 1:
        raise Fail(f"Nexus mod file contains multiple versions named {version}")
    return matches[0] if matches else None


def nexus_test_file(files, name):
    matches = [file for file in files if file["name"] == name]
    if len(matches) > 1:
        raise Fail(f"Nexus test mod contains multiple files named {name}")
    return matches[0] if matches else None


def nexus_release(entry, version, lines, key, archive, *, test_name=None):
    mod_url = f"{API}/v3/mods/{entry['internalId']}"
    notes_url = f"{API}/v1/games/eldenring/mods/{entry['id']}/changelogs.json"
    mod = call("GET", mod_url, key)["data"]
    if test_name and (mod["status"] not in NEXUS_TEST_STATUSES or str(mod["game_scoped_id"]) != str(entry["id"])):
        raise Fail("The Nexus test target must be the configured unpublished or hidden mod")
    files = call("GET", f"{mod_url}/files", key)["data"]["mod_files"]
    if test_name:
        name = test_name
        chosen = nexus_test_file(files, name)
    else:
        chosen, name = pick_file(entry, files, None, True)
    if chosen and not chosen["is_active"]:
        raise Fail(f"Nexus file {chosen['id']} is inactive")
    file_id = chosen["id"] if chosen else None
    file_url = f"{API}/v3/mod-files/{file_id}/versions"
    current = mod["version"]
    if archive and re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", current or ""):
        if tuple(map(int, current.split("."))) > tuple(map(int, version.split("."))):
            raise Fail(f"Nexus already has newer mod version {current}")

    uncached = {"Cache-Control": "no-cache"}
    notes = call("GET", notes_url, key, headers=uncached).get(version, [])
    if notes and notes != lines:
        raise Fail(f"Nexus changelog [{version}] differs from the repository")
    released = nexus_file_version(key, file_url, version) if file_id else None
    if not released and archive:
        print(f"Uploading {archive.name} to Nexus mod {entry['id']}", flush=True)
        attach_version(key, file_id or entry["internalId"], name, version, "main", archive,
                       lines, new_file=file_id is None)
        print(f"Nexus accepted version {version}; checking the published file", flush=True)
        for _ in range(5):
            if not file_id:
                # The create response identifies the new version. The list gives its file group.
                files = call("GET", f"{mod_url}/files", key)["data"]["mod_files"]
                chosen = nexus_test_file(files, name)
                if chosen:
                    file_id = chosen["id"]
                    file_url = f"{API}/v3/mod-files/{file_id}/versions"
            if file_id:
                released = nexus_file_version(key, file_url, version)
            mod = call("GET", mod_url, key)["data"]
            if released and mod["version"] == version:
                break
            time.sleep(3)
    if not released:
        raise Fail(f"Nexus mod {entry['id']} does not contain file {name} version {version}")
    if test_name and mod["status"] not in NEXUS_TEST_STATUSES:
        raise Fail("The Nexus test mod is no longer unpublished or hidden")
    primary = mod["main_file_version"] or {}
    is_primary = released.get("is_primary") if test_name else (
        primary.get("id") == released["id"] and primary.get("is_primary")
    )
    if (released["category"] != "main" or mod["version"] != version
            or not is_primary):
        raise Fail(f"Nexus has {version}, but it is not the active primary main release")
    if archive and (released["size_in_bytes"] != archive.stat().st_size or released["name"] != name):
        raise Fail(f"Nexus version {version} does not match the release archive size or name")

    if not notes and archive:
        print(f"Adding Nexus changelog for {version}", flush=True)
        call("POST", f"{mod_url}/changelogs", key,
             body={"version": version, "changelog": "\n".join(lines)})
        for _ in range(5):
            notes = call("GET", notes_url, key, headers=uncached).get(version, [])
            if notes:
                break
            time.sleep(3)
    if notes != lines:
        raise Fail(f"Nexus changelog [{version}] does not match the repository")
    if archive:
        description = re.sub(r"<br\s*/?>", "\n", released["description"] or "", flags=re.I)
        if description.splitlines() != file_description(lines).splitlines():
            raise Fail(f"Nexus file description for {version} does not match the release")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("crate")
    parser.add_argument("--version")
    parser.add_argument("--file-id")
    parser.add_argument("--category", choices=["main", "optional", "miscellaneous"], default="main")
    parser.add_argument("--dry-run", action="store_true", help="build and report, upload nothing")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--archive", type=Path, help="publish an existing GitHub Release ZIP")
    mode.add_argument("--verify-only", action="store_true", help="verify the Nexus main release")
    parser.add_argument("--test-nexus", action="store_true", help="use the unpublished or hidden Nexus test mod")
    args = parser.parse_args()

    github_release = args.archive is not None or args.verify_only
    if args.test_nexus and not github_release:
        parser.error("--test-nexus requires --archive or --verify-only")
    if github_release and (args.category != "main" or args.file_id or args.dry_run):
        parser.error("--archive and --verify-only use the configured main Nexus file")
    manifest_path = NEXUS_MODS if github_release else MANIFEST
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    entry = next((e for e in manifest if e["crate"] == args.crate), None)
    if entry is None:
        raise Fail(f"{args.crate} has no Nexus entry in {manifest_path.name}")
    cargo_version, lib = cargo_info(args.crate)
    version = args.version or cargo_version
    if version != cargo_version:
        raise Fail(f"Cargo.toml says {cargo_version}, not {version}")
    lines = changelog_lines(args.crate, version)
    if github_release:
        key = api_key()
        if args.archive:
            if args.archive.name != f"{lib}-{version}.zip" or not args.archive.is_file():
                raise Fail(f"Expected the existing {lib}-{version}.zip release archive")
        target = NEXUS_TEST_MOD if args.test_nexus else entry
        nexus_release(target, version, lines, key, args.archive, test_name=lib if args.test_nexus else None)
        print(f"Verified Nexus {version}: https://www.nexusmods.com/eldenring/mods/{target['id']}")
        return

    main_file = args.category == "main"
    if main_file and any(d["version"] == version for d in entry["deploys"]) and not args.dry_run:
        raise Fail(f"{version} is already in the manifest of {args.crate}")
    if git("status", "--porcelain", "--", f"mods/{args.crate}", "Cargo.lock").strip():
        raise Fail(f"uncommitted changes in mods/{args.crate} or Cargo.lock: commit them first")
    commit = git("rev-parse", "HEAD").strip()

    key = api_key()
    files = call("GET", f"{API}/v3/mods/{entry['internalId']}/files", key)["data"]["mod_files"]
    chosen, name = pick_file(entry, files, args.file_id, main_file)

    zip_path = build(args.crate, lib, version)
    changelog = "\n".join(lines)
    print(f"mod        {entry['name']} ({entry['id']}, internal {entry['internalId']})")
    print(f"version    {version}  commit {commit[:7]}")
    print(f"mod file   {chosen['id']}  {args.category}, versions will be named '{name}'")
    print(f"zip        {zip_path.relative_to(ROOT)}  {zip_path.stat().st_size} bytes")
    print("changelog\n" + "\n".join(f"  {line}" for line in lines))
    if args.dry_run:
        print("dry run: nothing was uploaded")
        return

    attach_version(key, chosen["id"], name, version, args.category, zip_path, lines)
    if main_file:
        entry["deploys"].append({"version": version, "commit": commit})
        entry["file"] = {"id": str(chosen["id"]), "name": name}
    else:
        entry["deploys"].append({"version": version, "commit": commit, "file": str(chosen["id"]),
                                 "category": args.category})
    MANIFEST.write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    if not main_file:
        print(f"published  https://www.nexusmods.com/eldenring/mods/{entry['id']}")
        return
    try:
        call("POST", f"{API}/v3/mods/{entry['internalId']}/changelogs", key,
             body={"version": version, "changelog": changelog})
    except Fail as error:
        raise Fail(f"version attached and manifest updated, but the changelog was not pushed: {error}", 3)
    print(f"published  https://www.nexusmods.com/eldenring/mods/{entry['id']}")


if __name__ == "__main__":
    try:
        main()
    except Fail as failure:
        print(f"error: {failure}", file=sys.stderr)
        sys.exit(failure.code)
