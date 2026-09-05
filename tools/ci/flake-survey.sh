#!/usr/bin/env bash
# Survey ci.yml flakiness over the last N runs: conclusion tally, reruns
# (with each rerun's attempt-1 failure cause — the honest flake sample,
# since a rerun's *final* attempt is by definition the one that passed),
# failed-run causes, and job durations for the run that actually
# exercises the heavy e2e lane. Plain-text output, meant to be pasted
# into a PR body.
#
# Usage: tools/ci/flake-survey.sh [LIMIT]   (default 60)
#
# Ports and fixes `~/.claude/plans/roost/ci-flake-survey.sh` (plan 047,
# §2.5): that script (a) let an in-progress run's null conclusion shift
# an awk column split, (b) picked the latest *green* main run for
# durations regardless of path filtering, which could be one where every
# heavy job was skipped (so the printed "duration" was really a
# skip-to-skip gap), and (c) never looked past a rerun's final, passing
# attempt, so it had no record of what the rerun was rerunning. Fetching
# `attempt`/`conclusion`/`status` from one `gh run list` call (rather
# than one `gh api` round trip per run) sidesteps (a) structurally and
# is why this version is a single JSON blob sliced by jq, not
# whitespace-split columns.
#
# Requires: gh (authed), jq, python3.
set -euo pipefail

REPO="charliek/roost"
LIMIT="${1:-60}"
# The four markers plan 047 settled on: enough to name what broke
# without pulling in noise like assertion internals or infra retries.
MARKERS='\.\.\. FAILED|panicked at|FAILED tools/|Text file busy'

# Reads one value per output LINE (not @tsv joined by tabs) into the
# FIELDS array. Tab counts as "IFS whitespace" to bash's own splitter —
# it collapses runs of it and trims it at the edges regardless of what
# IFS is set to — so `IFS=$'\t' read -r a b c` silently drops an empty
# middle field (e.g. a deleted source branch's empty headBranch) and
# shifts every column after it. One value per line has no such column to
# lose: an empty field is just an empty line, consumed like any other.
read_fields() {
  FIELDS=()
  local line
  while IFS= read -r line; do
    FIELDS+=("${line}")
  done
}

# Prints a run's marker lines from its failed-step log (same pipeline for a
# rerun's attempt-1 log and a failed run's own log — only the `gh run view`
# args and the not-found message differ). Captures gh's output before
# filtering so `head -6` truncating a long match list can't SIGPIPE `sort`
# and trip the same fallback that means "no markers found" — and so a
# genuine `gh` failure (auth/network) prints its own message instead of
# being indistinguishable from a clean log.
print_markers() {
  local id="$1" extra="$2" fallback="$3"
  local raw gh_status=0 marked
  # shellcheck disable=SC2086 # extra is a deliberate word-split gh arg (or empty)
  raw="$(gh run view -R "${REPO}" "${id}" ${extra} --log-failed 2>/dev/null)" || gh_status=$?
  if [ "${gh_status}" -ne 0 ]; then
    echo "   (gh run view --log-failed failed for ${id} — not necessarily a clean run; check it directly)"
    return
  fi
  marked="$(printf '%s\n' "${raw}" | grep -E "${MARKERS}" | sed -E 's/^[^Z]*Z //' | cut -c1-150 | sort -u | head -6)" || true
  if [ -z "${marked}" ]; then
    echo "   (${fallback})"
  else
    printf '%s\n' "${marked}" | sed 's/^/   /'
  fi
}

echo "==> fetching last ${LIMIT} ci.yml runs (repo=${REPO})" >&2
RUNS_JSON="$(gh run list -R "${REPO}" --workflow ci.yml --limit "${LIMIT}" \
  --json databaseId,conclusion,headBranch,createdAt,headSha,status,attempt)"

TOTAL=$(echo "${RUNS_JSON}" | jq 'length')
COMPLETED_JSON=$(echo "${RUNS_JSON}" | jq -c '[.[] | select(.status == "completed")]')
COMPLETED_COUNT=$(echo "${COMPLETED_JSON}" | jq 'length')
IN_PROGRESS_COUNT=$((TOTAL - COMPLETED_COUNT))

echo "=== last ${LIMIT} ci.yml runs by conclusion ==="
echo "${COMPLETED_JSON}" | jq -r '.[].conclusion' | sort | uniq -c
if [ "${IN_PROGRESS_COUNT}" -gt 0 ]; then
  echo "(${IN_PROGRESS_COUNT} of ${TOTAL} runs excluded: no conclusion yet)"
fi
echo

echo "=== reruns (run_attempt > 1 means someone hit rerun) ==="
RERUNS=$(echo "${COMPLETED_JSON}" | jq -c '[.[] | select(.attempt > 1)]')
RERUN_COUNT=$(echo "${RERUNS}" | jq 'length')
if [ "${RERUN_COUNT}" -eq 0 ]; then
  echo "(none in this window)"
fi
echo "${RERUNS}" | jq -c '.[]' | while IFS= read -r run; do
  read_fields < <(echo "${run}" \
    | jq -r '.databaseId, .attempt, .conclusion, .createdAt[0:10], .headBranch[0:40], .headSha[0:7]')
  id="${FIELDS[0]}" att="${FIELDS[1]}" concl="${FIELDS[2]}"
  date="${FIELDS[3]}" branch="${FIELDS[4]}" sha="${FIELDS[5]}"
  echo "${id} attempts=${att} final=${concl} ${date} ${branch} ${sha}"

  failed_jobs=$(gh api "repos/${REPO}/actions/runs/${id}/attempts/1/jobs" \
    --jq '.jobs[] | select(.conclusion=="failure") | .name' 2>/dev/null || true)
  if [ -z "${failed_jobs}" ]; then
    echo "   (attempt 1 job list unavailable — logs may have expired)"
    continue
  fi
  awk '{print "   job: " $0}' <<< "${failed_jobs}"
  print_markers "${id}" "--attempt 1" "no marker line in attempt 1's failed-step log — see the run for the cause"
done
echo

echo "=== failed runs: failed jobs + causes ==="
echo "${COMPLETED_JSON}" | jq -c '.[] | select(.conclusion=="failure")' | while IFS= read -r run; do
  read_fields < <(echo "${run}" \
    | jq -r '.databaseId, .createdAt[0:10], .headBranch[0:40], .headSha[0:7]')
  id="${FIELDS[0]}" date="${FIELDS[1]}" branch="${FIELDS[2]}" sha="${FIELDS[3]}"
  echo "== ${id} ${date} ${branch} ${sha}"
  gh run view -R "${REPO}" "${id}" --json jobs \
    --jq '.jobs[] | select(.conclusion=="failure") | "   job: \(.name)"' \
    || echo "   (job list unavailable)"
  print_markers "${id}" "" "no marker line in the failed-step log — see the run for the cause"
done
echo

echo "=== job durations: latest main run where iced-build-e2e actually ran ==="
DURATIONS_RUN=""
DURATIONS_JOBS=""
MAIN_CANDIDATES=$(echo "${COMPLETED_JSON}" | jq -c '[.[] | select(.headBranch=="main")] | sort_by(.createdAt) | reverse | .[]')
while IFS= read -r run; do
  [ -n "${run}" ] || continue
  id=$(echo "${run}" | jq -r '.databaseId')
  jobs=$(gh run view -R "${REPO}" "${id}" --json jobs --jq '.jobs' 2>/dev/null || echo '[]')
  # "ran" means a real conclusion with real timestamps, not just
  # "not skipped" — a job cancelled before it started is neither
  # skipped nor a genuine sample of the lane's duration.
  ran=$(echo "${jobs}" | jq '[.[] | select(.name | test("iced-build-e2e"))
    | select(.conclusion == "success" or .conclusion == "failure")
    | select(.startedAt != null and .completedAt != null)] | length')
  if [ "${ran}" -gt 0 ]; then
    DURATIONS_RUN="${id}"
    DURATIONS_JOBS="${jobs}"
    break
  fi
done <<< "${MAIN_CANDIDATES}"

if [ -z "${DURATIONS_RUN}" ]; then
  echo "(no main run in this window ran iced-build-e2e — widen LIMIT)"
else
  echo "run ${DURATIONS_RUN}"
  SKIPPED=$(echo "${DURATIONS_JOBS}" | jq '[.[] | select(.conclusion=="skipped")] | length')
  [ "${SKIPPED}" -eq 0 ] || echo "(${SKIPPED} skipped jobs omitted)"
  echo "${DURATIONS_JOBS}" \
    | jq -r '.[] | select(.conclusion != "skipped" and .startedAt != null and .completedAt != null) | "\(.name)\t\(.startedAt)\t\(.completedAt)"' \
    | python3 -c '
import sys, datetime
rows = []
for line in sys.stdin:
    name, s, e = line.rstrip("\n").split("\t")
    d = (datetime.datetime.fromisoformat(e.replace("Z", "+00:00"))
         - datetime.datetime.fromisoformat(s.replace("Z", "+00:00"))).total_seconds()
    rows.append((d, name))
for d, name in sorted(rows, reverse=True):
    print(f"   {int(d)//60:2d}m{int(d)%60:02d}s  {name}")
'
fi
