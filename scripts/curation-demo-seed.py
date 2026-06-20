#!/usr/bin/env python3
"""Seed the local curation demo: admin login, create pypi local/remote/virtual
repos, attach a min-age curation policy to the Remote.

Run from inside the compose network (reaches the backend at backend:8080):
    docker exec -i ak-curation-mock-webhook python3 - < scripts/curation-demo-seed.py

Env:
    BASE          default http://backend:8080
    MIN_AGE_DAYS  default 3650  (huge -> every upstream version blocked, a
                  deterministic demo; set 0 to allow everything)
"""
import base64
import json
import os
import urllib.request
import urllib.error


def jwt_sub(token):
    """Extract the `sub` (user id) claim from a JWT without verifying."""
    payload = token.split(".")[1]
    payload += "=" * (-len(payload) % 4)
    return json.loads(base64.urlsafe_b64decode(payload)).get("sub")

BASE = os.environ.get("BASE", "http://backend:8080")
MIN_AGE_DAYS = int(os.environ.get("MIN_AGE_DAYS", "3650"))


def call(method, path, token=None, body=None):
    url = f"{BASE}{path}"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Content-Type", "application/json")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(req) as r:
            raw = r.read().decode()
            return r.status, (json.loads(raw) if raw else {})
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        return e.code, raw


def main():
    admin_pw = os.environ.get("ADMIN_PASSWORD", "ChangeMe-Curation-12345!")
    # The backend provisions admin from a secure ADMIN_PASSWORD with
    # must_change=false, so the API is unlocked on boot — log in directly.
    st, resp = call("POST", "/api/v1/auth/login",
                    body={"username": "admin", "password": admin_pw})
    if st != 200:
        print(f"login failed: {st} {resp}")
        return
    token = resp["access_token"]
    print("logged in as admin")

    # 2. Create repos (idempotent-ish: ignore 409 conflicts)
    repos = [
        {"key": "pypi-local", "name": "pypi-local", "format": "pypi", "repo_type": "local"},
        {"key": "pypi-remote", "name": "pypi-remote", "format": "pypi",
         "repo_type": "remote", "upstream_url": "https://pypi.org"},
        {"key": "pypi-virtual", "name": "pypi-virtual", "format": "pypi",
         "repo_type": "virtual",
         "member_repos": [
             {"repo_key": "pypi-local", "priority": 1},
             {"repo_key": "pypi-remote", "priority": 2},
         ]},
    ]
    ids = {}
    for r in repos:
        st, resp = call("POST", "/api/v1/repositories", token, r)
        if st in (200, 201):
            ids[r["key"]] = resp.get("id")
            print(f"created {r['key']} ({r['repo_type']}) id={resp.get('id')}")
        elif st == 409:
            print(f"{r['key']} already exists; fetching id")
        else:
            print(f"create {r['key']} -> {st} {resp}")

    # Resolve remote id if it already existed
    if "pypi-remote" not in ids:
        st, resp = call("GET", "/api/v1/repositories", token)
        if st == 200:
            for rr in resp if isinstance(resp, list) else resp.get("items", []):
                if rr.get("key") == "pypi-remote":
                    ids["pypi-remote"] = rr.get("id")
    remote_id = ids.get("pypi-remote")
    if not remote_id:
        print("could not resolve pypi-remote id; aborting policy step")
        return

    # 3. Attach curation policy to the Remote
    policy = {
        "enabled": True,
        "min_age_enabled": True,
        "min_age_days": MIN_AGE_DAYS,
        "webhook_enabled": False,
        "webhook_fail_mode": "closed",
        "default_action": "allow",
    }
    st, resp = call("PUT", f"/api/v1/curation/policies/{remote_id}", token, policy)
    print(f"policy PUT -> {st}")
    if st == 200:
        print(json.dumps(resp, indent=2) if isinstance(resp, dict) else resp)

    print("\nDONE. Try a download through the virtual repo (host):")
    print("  pip download --no-deps --index-url http://localhost:18080/pypi/pypi-virtual/simple/ requests")
    print(f"  min_age_days={MIN_AGE_DAYS} -> expect 403 'blocked by curation policy' at the wheel fetch")


if __name__ == "__main__":
    main()
