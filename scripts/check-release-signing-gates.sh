#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Checks which workflow jobs can read the production updater signing key, and
# what may trigger them.
#
# Rules, enforced on each parsed workflow file:
#   1. A job that reads a secret other than GITHUB_TOKEN must declare
#      `environment: release-signing`. Only a job that reads such a secret may
#      declare that environment. No other environment may be declared.
#   2. A workflow may not read such a secret at its top level, because that
#      would reach every job.
#   3. A workflow that reads such a secret may be triggered only by `push` or
#      `workflow_dispatch`.
#   4. If it can be triggered by `push`, its `preflight` job must run a full-history
#      checkout and then, unconditionally, scripts/check-release-tag-on-main.sh
#      with the tag. Each reading job must need `preflight`.
#
# A YAML parser reads the files, so quoted keys, flow style, comments, and
# expressions are handled as GitHub reads them. The check is still a structural
# check. It does not see GitHub's settings: the environment's reviewers, its
# deployment branch and tag policy, and the tag ruleset.
#
# Requires python3 with PyYAML.
#
# usage: scripts/check-release-signing-gates.sh [workflow-file...]
#        With no arguments, checks every .github/workflows/*.yml and *.yaml.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [ "$#" -eq 0 ]; then
  shopt -s nullglob
  set -- "$root"/.github/workflows/*.yml "$root"/.github/workflows/*.yaml
  shopt -u nullglob
  [ "$#" -gt 0 ] || { echo "INPUT ERROR: no workflow files found" >&2; exit 2; }
fi

python3 -I - "$@" <<'PYEOF'
import re
import sys

try:
    import yaml
except ImportError:
    print("INPUT ERROR: python3 with PyYAML is required to read the workflow files", file=sys.stderr)
    sys.exit(2)

ENVIRONMENT = "release-signing"
ALLOWED_TRIGGERS = {"push", "workflow_dispatch"}
violations = []


def refuse(name, message):
    violations.append(f"FAIL {name}: {message}")


def expressions(text):
    """The expression bodies in a string: ${{ ... }} parts only."""
    return re.findall(r"\$\{\{(.*?)\}\}", text, re.S)


def reads_signing_secret(expression):
    """True if an expression reads a secret other than GITHUB_TOKEN."""
    stripped = expression.replace("secrets.GITHUB_TOKEN", "")
    return re.search(r"\bsecrets\b", stripped) is not None


def strings(node, key=None):
    """Every string in a parsed node, paired with its key, for expression checks."""
    if isinstance(node, dict):
        for k, v in node.items():
            yield from strings(v, k)
    elif isinstance(node, list):
        for item in node:
            yield from strings(item, key)
    elif isinstance(node, str):
        yield key, node


def node_reads_secret(node):
    for key, text in strings(node):
        if key == "if" and reads_signing_secret(text):
            return True
        if any(reads_signing_secret(e) for e in expressions(text)):
            return True
    return False


def job_key_reads_secret(job):
    # `secrets:` on a job passes secrets to a called workflow.
    return isinstance(job, dict) and "secrets" in job


def triggers_of(data):
    on = data.get("on", data.get(True))
    if on is None:
        return set()
    if isinstance(on, str):
        return {on}
    if isinstance(on, list):
        return set(on)
    if isinstance(on, dict):
        return set(on.keys())
    return set()


def environment_of(job):
    env = job.get("environment")
    if env is None:
        return None
    if isinstance(env, str):
        return env
    if isinstance(env, dict):
        return env.get("name")
    return None


def checkout_depth_is_full(step):
    uses = step.get("uses", "")
    depth = (step.get("with") or {}).get("fetch-depth")
    return uses.startswith("actions/checkout") and str(depth) == "0"


def runs_tag_check(step):
    run = step.get("run")
    if not isinstance(run, str):
        return False
    for line in run.splitlines():
        if line.lstrip().startswith("#"):
            continue
        if re.match(r'^\s*scripts/check-release-tag-on-main\.sh\s+"?\$TAG"?\s*$', line):
            return True
    return False


def check_file(path):
    name = path.rsplit("/", 1)[-1]
    try:
        with open(path, encoding="utf-8") as handle:
            data = yaml.safe_load(handle) or {}
    except (OSError, yaml.YAMLError) as error:
        violations.append(f"FAIL {name}: cannot parse the workflow: {error}")
        return
    if not isinstance(data, dict):
        violations.append(f"FAIL {name}: not a workflow mapping")
        return

    if node_reads_secret(data.get("env")):
        refuse(name, "the top-level env reads a secret other than GITHUB_TOKEN, so every job would receive it")

    jobs = data.get("jobs") or {}
    readers = []
    gated = []
    for job_id, job in jobs.items():
        if not isinstance(job, dict):
            refuse(name, f"job {job_id} is not a mapping")
            continue
        reads = node_reads_secret(job) or job_key_reads_secret(job)
        if reads:
            readers.append(job_id)
        declared = job.get("environment")
        if declared is not None:
            env_name = environment_of(job)
            if env_name != ENVIRONMENT:
                refuse(name, f"job {job_id} declares an unrecognized environment {declared!r}")
            else:
                gated.append(job_id)

    for job_id in readers:
        if job_id not in gated:
            refuse(name, f"job {job_id} reads the production signing key without 'environment: {ENVIRONMENT}'")
    for job_id in gated:
        if job_id not in readers:
            refuse(name, f"job {job_id} declares 'environment: {ENVIRONMENT}' but does not read the signing key")

    if not readers:
        print(f"ok   {name}: no job reads the production signing key")
        return

    triggers = triggers_of(data)
    for trigger in sorted(triggers - ALLOWED_TRIGGERS):
        refuse(name, f"a workflow that reads the signing key may not run on '{trigger}'")

    if "push" in triggers:
        preflight = jobs.get("preflight")
        if not isinstance(preflight, dict):
            refuse(name, "a push-triggered workflow reads the signing key but has no preflight job")
        else:
            if "if" in preflight:
                refuse(name, "the preflight job has an 'if', so it may be skipped")
            steps = preflight.get("steps") or []
            depth_at = next((i for i, s in enumerate(steps) if isinstance(s, dict) and checkout_depth_is_full(s)), None)
            check_at = next((i for i, s in enumerate(steps) if isinstance(s, dict) and runs_tag_check(s)), None)
            if depth_at is None:
                refuse(name, "the preflight job has no full-history checkout (actions/checkout with fetch-depth: 0)")
            if check_at is None:
                refuse(name, "the preflight job does not run scripts/check-release-tag-on-main.sh \"$TAG\"")
            elif depth_at is not None and depth_at > check_at:
                refuse(name, "the full-history checkout comes after the tag check")
            elif check_at is not None:
                step = steps[check_at]
                if any(k in step for k in ("if", "continue-on-error")):
                    refuse(name, "the tag check step has 'if' or 'continue-on-error', so it may not gate the run")
        for job_id in readers:
            needs = jobs[job_id].get("needs") or []
            needs = [needs] if isinstance(needs, str) else needs
            if "preflight" not in needs:
                refuse(name, f"job {job_id} reads the signing key but does not need preflight")

    if not any(v.startswith(f"FAIL {name}:") for v in violations):
        print(f"ok   {name}: jobs reading the signing key: [{', '.join(readers)}]")


for path in sys.argv[1:]:
    check_file(path)

if violations:
    for line in violations:
        print(line, file=sys.stderr)
    print(f"FAIL {len(violations)} release signing gate violation(s)", file=sys.stderr)
    sys.exit(1)
PYEOF
