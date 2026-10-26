#!/usr/bin/env python3
"""Push local HEAD to GitHub via the Git Data API (api.github.com).

Fallback for when github.com:443 is unreachable but api.github.com works.
Handles divergent shas: if the remote head is not in local ancestry (e.g. a
previous API push created a different sha), it matches the remote tree to a
local commit with an identical tree and diffs against that instead.
Usage: python gh_api_push.py <owner/repo> [branch]
"""
import base64
import json
import subprocess
import sys


def gh_api(method, endpoint, payload=None):
    cmd = ["gh", "api", endpoint, "-X", method]
    if payload is not None:
        cmd += ["--input", "-"]
    r = subprocess.run(cmd, input=json.dumps(payload) if payload else None,
                       capture_output=True, text=True, encoding="utf-8")
    if r.returncode != 0:
        raise RuntimeError(f"gh api {endpoint} failed: {r.stderr.strip()}")
    return json.loads(r.stdout)


def git(*args):
    return subprocess.run(["git", *args], capture_output=True, text=True,
                          encoding="utf-8", check=True).stdout.strip()


def find_diff_base(remote_sha, remote_tree):
    """Local ref to diff against: remote_sha if known, else a local commit
    whose tree matches the remote head tree, else None (full upload)."""
    have = subprocess.run(["git", "cat-file", "-t", remote_sha],
                          capture_output=True, text=True)
    if have.returncode == 0:
        return remote_sha
    for sha in git("rev-list", "HEAD").splitlines():
        if git("rev-parse", f"{sha}^{{tree}}") == remote_tree:
            print(f"remote sha unknown locally; tree matches local {sha[:8]}, diffing against it")
            return sha
    return None


def main():
    repo_full = sys.argv[1]
    branch = sys.argv[2] if len(sys.argv) > 2 else "main"
    ref = gh_api("GET", f"/repos/{repo_full}/git/ref/heads/{branch}")
    remote_sha = ref["object"]["sha"]
    local_sha = git("rev-parse", "HEAD")
    if remote_sha == local_sha:
        print("already up to date")
        return
    remote_tree = gh_api("GET", f"/repos/{repo_full}/git/commits/{remote_sha}")["tree"]["sha"]
    local_tree = git("rev-parse", "HEAD^{tree}")
    if remote_tree == local_tree:
        # content identical, just fast-forward the ref locally? no - create
        # nothing; only re-point remote if it differs (it doesn't). done.
        print("remote tree already matches HEAD; nothing to push")
        return

    base = find_diff_base(remote_sha, remote_tree)
    if base:
        diff = git("diff-tree", "--no-commit-id", "--name-status", "-r", base, "HEAD")
        changes = [l.split("\t") for l in diff.splitlines()]
    else:
        print("no matching local tree; uploading full tree")
        changes = [["A", p] for p in git("ls-tree", "-r", "--name-only", "HEAD").splitlines()]

    tree_entries = []
    for parts in changes:
        status, path = parts[0], parts[-1]
        if status.startswith("D"):
            tree_entries.append({"path": path, "mode": "100644", "type": "blob", "sha": None})
            continue
        if status.startswith("R") and len(parts) == 3:
            tree_entries.append({"path": parts[1], "mode": "100644", "type": "blob", "sha": None})
        data = subprocess.run(["git", "show", f"HEAD:{path}"], capture_output=True, check=True).stdout
        blob = gh_api("POST", f"/repos/{repo_full}/git/blobs",
                      {"content": base64.b64encode(data).decode(), "encoding": "base64"})
        tree_entries.append({"path": path, "mode": "100644", "type": "blob", "sha": blob["sha"]})
        print(f"  blob {path}")

    message = git("log", "-1", "--format=%B")
    author = {"name": git("log", "-1", "--format=%an"),
              "email": git("log", "-1", "--format=%ae"),
              "date": git("log", "-1", "--format=%aI")}
    tree = gh_api("POST", f"/repos/{repo_full}/git/trees",
                  {"base_tree": remote_sha, "tree": tree_entries})
    commit = gh_api("POST", f"/repos/{repo_full}/git/commits", {
        "message": message, "tree": tree["sha"], "parents": [remote_sha],
        "author": author,
        "committer": {**author, "date": git("log", "-1", "--format=%cI")},
    })
    gh_api("PATCH", f"/repos/{repo_full}/git/refs/heads/{branch}", {"sha": commit["sha"]})
    print(f"pushed {remote_sha[:8]}..{commit['sha'][:8]} -> {branch}")


if __name__ == "__main__":
    main()
