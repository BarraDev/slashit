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
#      checkout and then, unconditionally, a step whose only command is
#      scripts/check-release-tag-on-main.sh with the tag. The preflight job and
#      that step may not declare `if` or `continue-on-error`. Each reading job
#      must need `preflight`, and may not declare `if`, because a condition can
#      run it after a failed preflight.
#
# The file is read with Ruby's Psych, the YAML parser that scripts/check-workflow-shells.sh
# also uses. Duplicate keys, merge keys, anchors, aliases, tags, and extra
# documents are refused, so the file has one meaning: the one GitHub reads.
# An expression runs from `${{` to the `}}` that ends it, outside any
# single-quoted literal, as GitHub reads it. An unterminated expression counts as
# a read of the key.
#
# The check is still structural. It does not see GitHub's settings: the
# environment's reviewers, its deployment branch and tag policy, and the tag ruleset.
#
# Requires ruby. Its YAML library is part of the standard library.
#
# usage: scripts/check-release-signing-gates.sh [workflow-file...]
#        With no arguments, checks every .github/workflows/*.yml and *.yaml.
set -euo pipefail

if ! command -v ruby >/dev/null 2>&1 || ! ruby -ryaml -e '' >/dev/null 2>&1; then
  echo "INPUT ERROR: ruby with its YAML library is required to read the workflow files" >&2
  exit 2
fi

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [ "$#" -eq 0 ]; then
  shopt -s nullglob
  set -- "$root"/.github/workflows/*.yml "$root"/.github/workflows/*.yaml
  shopt -u nullglob
  [ "$#" -gt 0 ] || { echo "INPUT ERROR: no workflow files found" >&2; exit 2; }
fi

ruby - "$@" <<'RUBYEOF'
require "yaml"

ENVIRONMENT = "release-signing"
ALLOWED_TRIGGERS = %w[push workflow_dispatch].freeze
TAG_REF = "${{ github.ref_name }}"
# Expression context names are case-insensitive on GitHub: SECRETS.X reads the same secret.
GITHUB_TOKEN_REF = /secrets\.GITHUB_TOKEN(?![A-Za-z0-9_])/i
SECRETS_WORD = /\bsecrets\b/i
TAG_CHECK = /\Ascripts\/check-release-tag-on-main\.sh\s+"?\$TAG"?\z/

def refuse(violations, name, message)
  violations << "FAIL #{name}: #{message}"
end

# The bodies of the `${{ ... }}` expressions in a string. Each runs from `${{` to
# the first `}}` outside a single-quoted literal, where `''` is an escaped quote.
# Returns nil for an unterminated expression.
def expression_bodies(text)
  bodies = []
  rest = text
  while (start = rest.index("${{"))
    index = start + 3
    quoted = false
    closed = nil
    while index < rest.length
      char = rest[index]
      if quoted
        if char == "'"
          if rest[index + 1] == "'"
            index += 2
            next
          end
          quoted = false
        end
      elsif char == "'"
        quoted = true
      elsif rest[index, 2] == "}}"
        closed = index
        break
      end
      index += 1
    end
    return nil if closed.nil?

    bodies << rest[(start + 3)...closed]
    rest = rest[(closed + 2)..]
  end
  bodies
end

def reads_signing_secret?(expression)
  expression.gsub(GITHUB_TOKEN_REF, "").match?(SECRETS_WORD)
end

# True if the node reads a secret other than GITHUB_TOKEN. An `if` value is an
# expression even without the `${{ }}` wrapper.
def reads_secret?(node, key = nil)
  case node
  when Hash
    node.any? { |child_key, child| reads_secret?(child, child_key) }
  when Array
    node.any? { |item| reads_secret?(item, key) }
  when String
    return true if key == "if" && reads_signing_secret?(node)

    bodies = expression_bodies(node)
    return true if bodies.nil?

    bodies.any? { |body| reads_signing_secret?(body) }
  else
    false
  end
end

def triggers_of(data)
  on = data.key?("on") ? data["on"] : data[true]
  case on
  when String then [on]
  when Array then on.map(&:to_s)
  when Hash then on.keys.map(&:to_s)
  else []
  end
end

def environment_of(job)
  env = job["environment"]
  case env
  when String then env
  when Hash then env["name"]
  end
end

def full_history_checkout?(step)
  with = step["with"]
  step["uses"].to_s.start_with?("actions/checkout") &&
    with.is_a?(Hash) && with["fetch-depth"].to_s == "0"
end

# The step's run is exactly the tag check. Blank and comment lines are ignored;
# any other line would be a command too, and could exit before the check.
def tag_check_step?(step)
  run = step["run"]
  return false unless run.is_a?(String)

  commands = run.lines.map(&:strip).reject { |line| line.empty? || line.start_with?("#") }
  commands.size == 1 && TAG_CHECK.match?(commands.first)
end

# Refused constructs are the ones a second parser could read differently from
# GitHub: an alias or merge key expands to keys GitHub may not expand, a tag
# changes a value's type, and a duplicate key is resolved last-wins by some
# parsers and rejected by others. A plain key that YAML 1.1 reads as a
# non-text value (yes, true, off, null, ~, 1) is refused too: two such keys can
# collapse into one in Psych, which hides the job or the value that lost. The
# root `on` is the one exception, because it is the trigger key.
def structure_errors(node, errors = [], root = false)
  if node.is_a?(Psych::Nodes::Alias)
    errors << "an alias (*name) is not allowed; write the value out"
  elsif node.respond_to?(:tag) && node.tag
    errors << "a tag (#{node.tag}) is not allowed"
  end

  if node.is_a?(Psych::Nodes::Mapping)
    keys = node.children.each_slice(2).map(&:first).grep(Psych::Nodes::Scalar)
    keys.each do |key|
      next if key.tag || key.to_ruby.is_a?(String)
      next if root && key.value == "on"

      errors << "key #{key.value.inspect} is read as a non-text value; quote it"
    end
    keys.map(&:value).group_by(&:itself).each do |key, same|
      errors << "a merge key (<<) is not allowed" if key == "<<"
      errors << "duplicate key #{key.inspect}" if same.size > 1
    end
  end

  container = node.is_a?(Psych::Nodes::Stream) || node.is_a?(Psych::Nodes::Document)
  node.children.each { |child| structure_errors(child, errors, container) } if node.respond_to?(:children) && node.children
  errors
end

# A job or workflow with actions: write can dispatch a signing workflow, which the
# tag gate does not see. Refused in every workflow file.
def actions_write?(permissions)
  permissions == "write-all" || (permissions.is_a?(Hash) && permissions["actions"].to_s == "write")
end

def check_preflight(violations, name, jobs, readers)
  preflight = jobs["preflight"]
  unless preflight.is_a?(Hash)
    refuse(violations, name, "a push-triggered workflow reads the signing key but has no preflight job")
    return
  end
  refuse(violations, name, "the preflight job has an 'if', so it may be skipped") if preflight.key?("if")
  if preflight.key?("continue-on-error")
    refuse(violations, name, "the preflight job has 'continue-on-error', so a failed check would not stop the readers")
  end
  if preflight.key?("defaults")
    refuse(violations, name, "the preflight job has 'defaults', which can change the shell that runs the tag check")
  end

  steps = preflight["steps"].is_a?(Array) ? preflight["steps"] : []
  depth_at = steps.index { |step| step.is_a?(Hash) && full_history_checkout?(step) }
  check_at = steps.index { |step| step.is_a?(Hash) && tag_check_step?(step) }
  if depth_at.nil?
    refuse(violations, name, "the preflight job has no full-history checkout (actions/checkout with fetch-depth: 0)")
  else
    depth_step = steps[depth_at]
    if depth_step.key?("if") || depth_step.key?("continue-on-error")
      refuse(violations, name, "the full-history checkout has 'if' or 'continue-on-error', so it may be skipped")
    end
    with = depth_step["with"]
    if with.is_a?(Hash) && (with.key?("ref") || with.key?("repository"))
      refuse(violations, name, "the full-history checkout names a 'ref' or 'repository', so it may not check out this repository's tag")
    end
  end
  if check_at.nil?
    refuse(violations, name, "the preflight job does not run scripts/check-release-tag-on-main.sh \"$TAG\"")
  elsif depth_at && depth_at > check_at
    refuse(violations, name, "the full-history checkout comes after the tag check")
  else
    check_step = steps[check_at]
    if check_step.key?("if") || check_step.key?("continue-on-error")
      refuse(violations, name, "the tag check step has 'if' or 'continue-on-error', so it may not gate the run")
    end
    if check_step.key?("shell")
      refuse(violations, name, "the tag check step declares 'shell', which can replace the command that runs")
    end
    env = check_step["env"].is_a?(Hash) ? check_step["env"] : {}
    refuse(violations, name, "the tag check step does not set TAG from github.ref_name") unless env["TAG"] == TAG_REF
  end

  readers.each do |job_id|
    needs = Array(jobs[job_id]["needs"])
    next if needs.include?("preflight")

    refuse(violations, name, "job #{job_id} reads the signing key but does not need preflight")
  end
end

def check_file(path, violations)
  name = File.basename(path)
  before = violations.size

  begin
    stream = Psych.parse_stream(File.read(path, encoding: "UTF-8"))
  rescue Psych::Exception, SystemCallError, EncodingError => error
    refuse(violations, name, "cannot parse the workflow: #{error.message}")
    return
  end
  errors = structure_errors(stream)
  errors << "more than one YAML document" if stream.children.size > 1
  errors.each { |error| refuse(violations, name, error) }
  return unless errors.empty?

  data = stream.children.first&.to_ruby
  unless data.is_a?(Hash)
    refuse(violations, name, "not a workflow mapping")
    return
  end

  if reads_secret?(data["env"])
    refuse(violations, name, "the top-level env reads a secret other than GITHUB_TOKEN, so every job would receive it")
  end

  jobs = data["jobs"] || {}
  unless jobs.is_a?(Hash)
    refuse(violations, name, "jobs is not a mapping")
    return
  end

  permission_holders = [data] + jobs.values.select { |job| job.is_a?(Hash) }
  if permission_holders.any? { |holder| actions_write?(holder["permissions"]) }
    refuse(violations, name, "a workflow or job grants 'actions: write', which can dispatch a signing workflow without the tag gate")
  end

  readers = []
  gated = []
  jobs.each do |job_id, job|
    unless job.is_a?(Hash)
      refuse(violations, name, "job #{job_id} is not a mapping")
      next
    end
    readers << job_id if reads_secret?(job) || job.key?("secrets")
    next unless job.key?("environment")

    if environment_of(job) == ENVIRONMENT
      gated << job_id
    else
      refuse(violations, name, "job #{job_id} declares an unrecognized environment #{job["environment"].inspect}")
    end
  end

  readers.each do |job_id|
    next if gated.include?(job_id)

    refuse(violations, name, "job #{job_id} reads the production signing key without 'environment: #{ENVIRONMENT}'")
  end
  gated.each do |job_id|
    next if readers.include?(job_id)

    refuse(violations, name, "job #{job_id} declares 'environment: #{ENVIRONMENT}' but does not read the signing key")
  end

  if readers.empty?
    puts "ok   #{name}: no job reads the production signing key" if violations.size == before
    return
  end

  readers.each do |job_id|
    next unless jobs[job_id].key?("if")

    refuse(violations, name, "job #{job_id} declares 'if', so it may run after a failed preflight")
  end
  refuse(violations, name, "the workflow declares no top-level 'permissions'; declare permissions: {} and grant each job its own") unless data.key?("permissions")
  refuse(violations, name, "the workflow declares 'defaults', which can change the shell of every job") if data.key?("defaults")

  triggers = triggers_of(data)
  (triggers - ALLOWED_TRIGGERS).sort.each do |trigger|
    refuse(violations, name, "a workflow that reads the signing key may not run on '#{trigger}'")
  end

  check_preflight(violations, name, jobs, readers) if triggers.include?("push")

  if violations.size == before
    puts "ok   #{name}: jobs reading the signing key: [#{readers.join(", ")}]"
  end
end

violations = []
ARGV.each { |path| check_file(path, violations) }

if violations.empty?
  exit 0
end
violations.each { |line| warn line }
warn "FAIL #{violations.size} release signing gate violation(s)"
exit 1
RUBYEOF
