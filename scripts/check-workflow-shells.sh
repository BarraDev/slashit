#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Checks that a `run:` step which may execute on a Windows runner names its
# shell whenever its script uses POSIX shell syntax. Windows runners default to
# PowerShell, so `set -euo pipefail` fails there, at the step, after the rest of
# the job has already run. A step that names `shell:` is correct on every runner.
#
# The check is a heuristic: it recognizes common POSIX constructs (`set -`,
# `$NAME`, `&&`, `||`, `>>`). A step that uses only commands both shells accept
# passes without `shell:`, which is the intent. scripts/test-check-workflow-shells.sh
# shows what is caught.
#
# A job may run on Windows when its `runs-on` names windows, or when it runs on a
# matrix value and some matrix entry names windows. Shells come from the step,
# then the job's `defaults.run`, then the workflow's `defaults.run`.
#
# usage: scripts/check-workflow-shells.sh [workflow-file...]
#        With no arguments, checks every .github/workflows/*.yml of this repository.
set -euo pipefail

if ! command -v ruby >/dev/null 2>&1; then
  echo "INPUT ERROR: ruby is required to read the workflow files" >&2
  exit 2
fi

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [ "$#" -eq 0 ]; then
  set -- "$root"/.github/workflows/*.yml
fi

ruby -ryaml -e '
POSIX_SYNTAX = [/^\s*set -/, /\$\{?[A-Za-z_]/, /&&/, /\|\|/, />>/].freeze

def windows_capable?(job)
  runs_on = job["runs-on"].to_s
  return true if runs_on.include?("windows")
  return false unless runs_on.include?("matrix.")

  (job.dig("strategy", "matrix") || {}).to_s.include?("windows")
end

violations = []
ARGV.each do |path|
  workflow = YAML.safe_load(File.read(path)) || {}
  workflow_shell = workflow.dig("defaults", "run", "shell")
  (workflow["jobs"] || {}).each do |job_id, job|
    next unless windows_capable?(job)

    job_shell = job.dig("defaults", "run", "shell")
    (job["steps"] || []).each_with_index do |step, index|
      next unless step.is_a?(Hash) && step["run"]
      next if step["shell"] || job_shell || workflow_shell

      syntax = POSIX_SYNTAX.find { |pattern| step["run"].to_s.match?(pattern) }
      next unless syntax

      name = step["name"] || "step #{index + 1}"
      violations << "#{path}: job #{job_id}, #{name}: POSIX shell syntax but no `shell:`; " \
                    "a Windows runner would run it in PowerShell"
    end
  end
end

if violations.empty?
  puts "ok   every Windows-capable run step with POSIX syntax names its shell (#{ARGV.size} workflow files)"
  exit 0
end
violations.each { |violation| warn "FAIL #{violation}" }
exit 1
' -- "$@"
