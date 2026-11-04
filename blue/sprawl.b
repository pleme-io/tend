@moduledoc """
sprawl: the judgement over every branch, stash, worktree and backup ref tend reads from a workspace. Report only: a verdict and a command as text, and nothing here changes a ref.

Each rule is a kousei entry, on, tuned, off or refused alone. The first standing rule that holds for an item names its verdict; a repo tend could not read is blind, never zero.

    verdict_of(judge([item("branch", {"ahead" => 0})], rules_of([]), ctx_fixture)[0]) #=> "landed"
"""

use("junjo", [:sort_stable_by])

use(
  "kousei",
  %i[
    entry
    field
    flag
    int_in
    kind_of
    list_of
    name_of
    one_of
    overlay
    required
    text
    value_of
  ]
)

use("moji", %i[glob_match? includes])

use("retsu", %i[contains find_first first flat_map is_empty size])

use("shuugou", %i[group_by unique])

def severities()
  %w[alert warn info]
end

def kinds()
  %w[branch remote-branch stash worktree backup-ref default-branch]
end

def ops()
  {
    "eq" => (a, b) -> a == b,
    "ne" => (a, b) -> a != b,
    "lt" => (a, b) -> number?(a) and a < b,
    "le" => (a, b) -> number?(a) and a <= b,
    "gt" => (a, b) -> number?(a) and a > b,
    "ge" => (a, b) -> number?(a) and a >= b,
    "glob" => (a, b) -> string?(a) and glob_match?(b, a),
    "contains_any" => (a, b) -> string?(a) and any?((s) -> includes(a, s), b)
  }
end

@doc """
holds?: One clause over an item: `[field, op, value]`. An unknown field holds for nothing but the op "unknown", so a count never read cannot satisfy a rule. A clause is always three long: a list of two-string lists reads as a map when it crosses JSON.

    holds?({"unique" => 0}, ["unique", "eq", 0]) #=> true
    holds?({}, ["unique", "eq", 0]) #=> false
    holds?({}, ["unique", "unknown", true]) #=> true
"""
def holds?(item, clause)
  v = get(item, clause[0])
  if clause[1] == "unknown"
    v.nil?
  elsif v.nil?
    false
  else
    get(ops, clause[1])(v, clause[2])
  end
end

def any_glob?(globs, v)
  string?(v) and any?((g) -> glob_match?(g, v), globs)
end

def within_scope?(item, scope)
  list_ok = fn(key, field)
    get(scope, key).nil? or any_glob?(get(scope, key), get(item, field))
  end
  age = item["age_hours"]
  list_ok("org", "org") and
    list_ok("repo", "repo") and
    list_ok("ref", "ref") and
    list_ok("author", "author") and
    (
      scope["min_age_hours"].nil? or
      number?(age) and age >= scope["min_age_hours"]
    ) and
    (
      scope["max_age_hours"].nil? or
      number?(age) and age < scope["max_age_hours"]
    )
end

def rule_holds?(item, rule)
  contains(rule["kinds"], item["kind"]) and
    within_scope?(item, rule["scope"] or {}) and
    every?((c) -> holds?(item, c), rule["when"] or [])
end

@doc """
landed: true when nothing on the ref is new to the base, false when something is, nil when a count was not read. A merge commit carries work the patch comparison does not see, so a branch of merges alone is not landed by it.

    landed({"ahead" => 0}) #=> true
    landed({"ahead" => 3, "unique" => 0, "merges" => 0}) #=> true
    landed({"ahead" => 3, "unique" => 0, "merges" => 1}) #=> false
    landed({"ahead" => 3, "diff_empty" => true}) #=> true
    landed({"ahead" => 3}) #=> nil
    landed({}) #=> nil
"""
def landed(item)
  ahead = item["ahead"]
  unique = item["unique"]
  merges = item["merges"]
  empty = item["diff_empty"]
  if ahead == 0 or empty == true
    if ahead.nil? and empty != true
      nil
    else
      true
    end
  elsif ahead.nil? or unique.nil? or merges.nil?
    nil
  else
    unique == 0 and merges == 0
  end
end

def carries_author?(item)
  contains(%w[branch remote-branch backup-ref default-branch], item["kind"])
end

@doc """
own?: Whether the operator wrote it. A stash or a worktree is local to this machine, so it is the operator's. A branch with no author read is not.

    own?({"kind" => "branch", "author" => "a"}, ["a"]) #=> true
    own?({"kind" => "branch", "author" => "b", "email" => "a"}, ["a"]) #=> true
    own?({"kind" => "branch", "author" => "b"}, ["a"]) #=> false
    own?({"kind" => "branch"}, ["a"]) #=> false
    own?({"kind" => "stash"}, []) #=> true
"""
def own?(item, own)
  if carries_author?(item)
    contains(own, item["author"]) or contains(own, item["email"])
  else
    true
  end
end

def enriched(item, ctx)
  committed = item["committed_unix"]
  age = if committed.nil?
    item["age_hours"]
  else
    floor((ctx["now"] - committed) / 3600)
  end
  own = own?(item, ctx["own"])
  org = item["org"]
  protected = contains(ctx["report_only_orgs"], org)
  why = if protected
    "#{org} is report-only"
  elsif not own
    "#{item["kind"]} by #{item["author"] or "an unread author"}"
  else
    ""
  end
  overlay(
    item,
    {
      "age_hours" => age,
      "landed" => landed(item),
      "own" => own,
      "report_only" => protected or not own,
      "report_why" => why
    }
  )
end

def fill(template, item)
  reduce(
    (text, key) -> replace(text, "{#{key}}", to_s(get(item, key))),
    template,
    %w[path repo ref branch base author]
  )
end

def look(item)
  kind = item["kind"]
  template = if kind == "stash"
    "git -C {path} stash show {ref}"
  elsif kind == "worktree"
    "git -C {path} worktree list"
  else
    "git -C {path} log --oneline -5 {ref}"
  end
  fill(template, item)
end

@doc """
next_of: The command a rule suggests. A protected item gets a read-only look whatever the rule says, so no operator rule can suggest deleting it.

    next_of({"report_only" => false, "ref" => "x"}, {"next" => "drop {ref}"}) #=> "drop x"
    next_of({"report_only" => false}, {}) #=> ""
"""
def next_of(item, rule)
  if item["report_only"]
    "report only (#{item["report_why"]}): #{look(item)}"
  else
    fill(rule["next"] or "", item)
  end
end

def scope_table()
  globs = list_of(text)
  [
    field("org", globs),
    field("repo", globs),
    field("ref", globs),
    field("author", globs),
    field("min_age_hours", int_in(0, 1_000_000)),
    field("max_age_hours", int_in(0, 1_000_000))
  ]
end

def clause_why(c)
  if not list?(c) or length(c) < 2
    "#{c} is not [field, op, value]"
  elsif c[1] == "unknown"
    nil
  elsif get(ops, c[1]).nil?
    "#{c[1]} is not an op: #{join(map(first, map_entries(ops)), ", ")}, unknown"
  elsif length(c) != 3
    "#{c} is not [field, op, value]"
  end
end

def scope_why(v)
  first(kousei::record_whys(scope_table, v))
end

def example_why(e)
  if not map?(e) or e["item"].nil? or e["expect"].nil?
    "#{e} is not {item:, expect:}"
  end
end

def some_examples(check)
  fn(v)
    if not list?(v) or is_empty(v)
      "a rule carries at least one example"
    else
      list_of(check)(v)
    end
  end
end

def schema()
  {
    rule: [
      required(field("verdict", text)),
      required(field("kinds", list_of(one_of(kinds)))),
      field("when", list_of(clause_why)),
      field("scope", scope_why),
      required(field("severity", one_of(severities))),
      field("next", text),
      field("announce", flag),
      required(field("examples", some_examples(example_why)))
    ]
  }
end

def stringed(v)
  if map?(v)
    reduce(
      (acc, p) -> assoc(acc, to_s(p[0]), stringed(p[1])),
      {},
      map_entries(v)
    )
  elsif list?(v) and not string?(v)
    map(stringed, v)
  else
    v
  end
end

def rule(name, fields)
  entry(:rule, name, stringed(fields))
end

def example(item, expect)
  {item:, expect:}
end

def branch_kinds()
  %w[branch remote-branch]
end

def builtin()
  [
    rule(
      "default-branch-diverged",
      {
        verdict: "default-branch-diverged",
        kinds: ["default-branch"],
        when: [["no_merge_base", "eq", true]],
        severity: "alert",
        next: "git -C {path} log --oneline --left-right {branch}...{base}",
        examples: [
          example(
            {kind: "default-branch", no_merge_base: true},
            "default-branch-diverged"
          ),
          example({kind: "default-branch"}, "unclassified")
        ]
      }
    ),
    rule(
      "dead-worktree",
      {
        verdict: "dead-worktree",
        kinds: ["worktree"],
        when: [["exists", "eq", false]],
        severity: "warn",
        next: "git -C {path} worktree prune",
        examples: [
          example({kind: "worktree", exists: false}, "dead-worktree"),
          example({kind: "worktree"}, "idle-worktree")
        ]
      }
    ),
    rule(
      "dirty-worktree",
      {
        verdict: "dirty-worktree",
        kinds: ["worktree"],
        when: [["dirty", "eq", true]],
        severity: "warn",
        next: "git -C {ref} status --short",
        examples: [
          example(
            {kind: "worktree", exists: true, dirty: true},
            "dirty-worktree"
          )
        ]
      }
    ),
    rule(
      "idle-worktree",
      {
        verdict: "idle-worktree",
        kinds: ["worktree"],
        severity: "info",
        next: "git -C {path} worktree remove {ref}",
        examples: [
          example(
            {kind: "worktree", exists: true, dirty: false},
            "idle-worktree"
          )
        ]
      }
    ),
    rule(
      "bulk-sweep-stash",
      {
        verdict: "bulk-sweep-stash",
        kinds: ["stash"],
        when: [
          [
            "message",
            "contains_any",
            %w[retire-spec-autostash- claude-fleet-clean-]
          ]
        ],
        severity: "info",
        next: "git -C {path} stash show {ref}",
        examples: [
          example(
            {
              kind: "stash",
              message: "On main: retire-spec-autostash-2026-10-01"
            },
            "bulk-sweep-stash"
          ),
          example(
            {kind: "stash", message: "On main: claude-fleet-clean-9"},
            "bulk-sweep-stash"
          ),
          example(
            {kind: "stash", message: "On main: my work"},
            "individual-stash"
          )
        ]
      }
    ),
    rule(
      "individual-stash",
      {
        verdict: "individual-stash",
        kinds: ["stash"],
        severity: "warn",
        next: "git -C {path} stash show -p {ref}",
        examples: [
          example({kind: "stash", message: "WIP on main"}, "individual-stash")
        ]
      }
    ),
    rule(
      "backup-ref",
      {
        verdict: "backup-ref",
        kinds: ["backup-ref"],
        severity: "info",
        next: "git -C {path} log --oneline -3 {ref}",
        examples: [
          example(
            {
              kind: "backup-ref",
              ref: "refs/backup/closed-2026-11-03/x",
              author: "someone"
            },
            "backup-ref"
          )
        ]
      }
    ),
    rule(
      "foreign-author",
      {
        verdict: "foreign-author",
        kinds: branch_kinds,
        when: [["own", "eq", false]],
        severity: "info",
        examples: [
          example(
            {kind: "branch", author: "someone", ahead: 0},
            "foreign-author"
          ),
          example({kind: "remote-branch", author: "someone"}, "foreign-author")
        ]
      }
    ),
    rule(
      "checked-out",
      {
        verdict: "checked-out",
        kinds: ["branch"],
        when: [["checked_out", "eq", true]],
        severity: "info",
        announce: false,
        examples: [
          example({kind: "branch", checked_out: true, ahead: 0}, "checked-out"),
          example(
            {kind: "remote-branch", checked_out: true, ahead: 0},
            "landed"
          )
        ]
      }
    ),
    rule(
      "active",
      {
        verdict: "active",
        kinds: branch_kinds,
        scope: {max_age_hours: 72},
        severity: "info",
        announce: false,
        examples: [
          example({kind: "branch", age_hours: 1, ahead: 4}, "active"),
          example({kind: "branch", age_hours: 72, ahead: 4}, "lingering")
        ]
      }
    ),
    rule(
      "landed",
      {
        verdict: "landed",
        kinds: ["branch"],
        when: [["landed", "eq", true]],
        severity: "warn",
        next: "git -C {path} branch -D {branch}",
        examples: [
          example({kind: "branch", ahead: 0}, "landed"),
          example({kind: "branch", ahead: 5, unique: 0, merges: 0}, "landed"),
          example(
            {kind: "branch", ahead: 5, unique: 2, merges: 0},
            "lingering"
          ),
          example({kind: "branch", ahead: 5}, "lingering")
        ]
      }
    ),
    rule(
      "landed-remote",
      {
        verdict: "landed",
        kinds: ["remote-branch"],
        when: [["landed", "eq", true]],
        severity: "warn",
        next: "git -C {path} push origin --delete {branch}",
        examples: [
          example({kind: "remote-branch", ahead: 5, diff_empty: true}, "landed")
        ]
      }
    ),
    rule(
      "lingering",
      {
        verdict: "lingering",
        kinds: branch_kinds,
        severity: "warn",
        next: "git -C {path} log --oneline {base}..{ref}",
        examples: [
          example(
            {kind: "branch", ahead: 56, unique: 56, merges: 0},
            "lingering"
          ),
          example({kind: "remote-branch"}, "lingering")
        ]
      }
    )
  ]
end

def lifted(e)
  if not map?(e)
    e
  else
    value = stringed(e["value"] or e[:value] or {})
    kind = to_s(e["entry"] or e[:entry])
    name = e["name"] or e[:name]
    target = :rule
    if kind == "rule"
      rule(name, value)
    elsif kind == "off"
      kousei::off(target, name)
    elsif kind == "tune"
      kousei::tune(target, name, value["fields"] or {})
    else
      entry(kind, name, value)
    end
  end
end

def plain?(e)
  map?(e) and kind_of(e) == :rule
end

@doc """
rules_of: The operator's entries over the built-in rules, judged. An operator rule goes first and a rule named like a built-in replaces it; off and tune reach both. Each bad entry is refused alone.

    size(rules_of([])[:rules]) > 0 #=> true
    rules_of([])[:refusals] #=> []
"""
def rules_of(entries)
  lifts = map(lifted, entries)
  named = map(name_of, filter(plain?, lifts))
  kept = filter((e) -> not contains(named, name_of(e)), builtin)
  judged = kousei::judge(
    schema,
    append(filter(plain?, lifts), kept, filter((e) -> not plain?(e), lifts))
  )
  {
    rules: map(
      (e) -> assoc(value_of(e), "name", name_of(e)),
      judged[:accepted]
    ),
    off: map(name_of, judged[:off]),
    refusals: map((r) -> "#{r[1]}", judged[:refusals])
  }
end

def wire_rules(entries)
  out = rules_of(entries)
  {
    "rules" => map((r) -> assoc(r, "examples", []), out[:rules]),
    "off" => out[:off],
    "refusals" => out[:refusals]
  }
end

def rank(severity)
  if severity == "alert"
    0
  elsif severity == "warn"
    1
  else
    2
  end
end

def unclassified()
  {"name" => "unclassified", "verdict" => "unclassified", "severity" => "warn"}
end

def classified(item, rules)
  find_first((r) -> rule_holds?(item, r), rules) or unclassified
end

def judged_row(raw, rules, ctx)
  item = enriched(raw, ctx)
  rule = classified(item, rules)
  [
    rule["verdict"],
    rule["name"],
    rule["severity"],
    next_of(item, rule),
    item["report_only"],
    rank(rule["severity"]),
    rule["announce"] != false,
    item["age_hours"],
    item["landed"]
  ]
end

@doc """
judge_items: One row per item, in order: verdict, rule, severity, next command, report-only, severity rank, announce, age in hours, landed.
"""
def judge_items(items, rules, ctx)
  map((i) -> judged_row(i, rules, ctx), items)
end

def verdict_of(row)
  first(row)
end

def severity_of(row)
  row[2]
end

def judge(items, standing, ctx)
  judge_items(items, standing[:rules], ctx)
end

def class_row(group)
  rows = group[1]
  {
    "verdict" => group[0],
    "severity" => first(rows)[1],
    "rank" => first(rows)[2],
    "announce" => first(rows)[3],
    "count" => size(rows),
    "repos" => size(unique(map((r) -> r[4], rows)))
  }
end

@doc """
classes: Per verdict the count and the repos that hold it, worst severity first, then largest. Each row is `[verdict, severity, rank, announce, repo]`.

    classes([]) #=> []
"""
def classes(rows)
  grouped = group_by(rows, first)
  counted = map(class_row, grouped)
  sort_stable_by((c) -> c["rank"] * 1_000_000 - c["count"], counted)
end

def plural(n, word)
  if n == 1
    "#{n} #{word}"
  else
    "#{n} #{word}s"
  end
end

def class_line(c)
  "tend sprawl: #{c["count"]} #{c["verdict"]} in #{plural(c["repos"], "repo")} (#{c["severity"]}); tend sprawl --verdict #{c["verdict"]}."
end

def age_words(secs)
  if secs < 60
    "#{secs}s"
  elsif secs < 3600
    "#{floor(secs / 60)}m"
  elsif secs < 86400
    "#{floor(secs / 3600)}h"
  else
    "#{floor(secs / 86400)}d"
  end
end

def stale_line(meta)
  "tend sprawl: the snapshot is #{age_words(meta["age_secs"])} old, so these counts may be behind. #{meta["why"]}"
end

def blind_line(meta)
  "tend sprawl: #{plural(meta["blind"], "repo")} blind of #{meta["total"]}: unread, not zero; tend sprawl --json lists them."
end

@doc """
session_lines: One line per class with a count, never per branch. A stale or partly read snapshot says so, and no snapshot is blind.

    session_lines([], {"snapshot" => false, "why" => "no scan yet."}) #=> ["tend sprawl: no snapshot, so branch and stash sprawl is unseen (blind). no scan yet."]
    session_lines([], {"snapshot" => true, "stale" => false, "blind" => 0, "total" => 2, "age_secs" => 1}) #=> []
"""
def session_lines(classes, meta)
  if not meta["snapshot"]
    [
      "tend sprawl: no snapshot, so branch and stash sprawl is unseen (blind). #{meta["why"]}"
    ]
  else
    head = if meta["stale"]
      [stale_line(meta)]
    else
      []
    end
    blind = if meta["blind"] > 0
      [blind_line(meta)]
    else
      []
    end
    shown = filter((c) -> c["announce"], classes)
    append(head, map(class_line, shown), blind)
  end
end

@doc """
status_line: The one line `tend status` prints.

    status_line([], {"snapshot" => false}) #=> "sprawl: blind (no snapshot; tend sprawl refresh)"
"""
def status_line(classes, meta)
  if not meta["snapshot"]
    "sprawl: blind (no snapshot; tend sprawl refresh)"
  else
    counted = fn(rank)
      reduce(
        fn(acc, c)
          if c["rank"] == rank
            acc + c["count"]
          else
            acc
          end
        end,
        0,
        classes
      )
    end
    state = if meta["stale"]
      "stale, "
    else
      ""
    end
    "sprawl: #{counted(0)} alert, #{counted(1)} warn, #{counted(2)} info; #{meta["read"]} of #{meta["total"]} repos read, #{meta["blind"]} blind; snapshot #{state}#{age_words(meta["age_secs"])} old"
  end
end

@doc """
outcome: What a query found. An unread repo in scope makes nothing blind, never empty.

    outcome(3, 0, true) #=> "found"
    outcome(3, 2, true) #=> "found"
    outcome(0, 0, true) #=> "empty"
    outcome(0, 1, true) #=> "blind"
    outcome(0, 0, false) #=> "blind"
"""
def outcome(matched, blind_in_scope, snapshot)
  if not snapshot
    "blind"
  elsif matched > 0
    "found"
  elsif blind_in_scope > 0
    "blind"
  else
    "empty"
  end
end

@doc """
house_orgs: Orgs whose repos are only ever reported. Configuration adds to this list and cannot empty it.

    house_orgs #=> ["akeylesslabs"]
"""
def house_orgs()
  ["akeylesslabs"]
end

@doc """
context: What judging needs from the host: the clock, who the operator is, and the orgs configured report-only on top of the house's.

    context(1, ["me"], ["x"])["report_only_orgs"] #=> ["akeylesslabs", "x"]
"""
def context(now, own, orgs)
  {
    "now" => now,
    "own" => own,
    "report_only_orgs" => unique(append(house_orgs, orgs))
  }
end

def ctx_fixture()
  context(1000 * 3600, ["self"], [])
end

def touched_line(group)
  rows = group[1]
  words = map(
    (p) -> "#{p[1]} #{p[0]}",
    map(
      (v) -> [v, size(filter((r) -> r[1] == v, rows))],
      unique(map((r) -> r[1], rows))
    )
  )
  "tend sprawl: #{group[0]} holds #{join(words, ", ")}; tend sprawl --repo #{group[0]}."
end

@doc """
touched_lines: One line per repo a session touched, with what it holds at warn or worse. Each row is `[repo, verdict, rank, announce]`.

    touched_lines([]) #=> []
"""
def touched_lines(rows)
  loud = filter((r) -> r[3] and r[2] <= 1, rows)
  map(touched_line, group_by(loud, first))
end

def item(kind, fields)
  overlay(
    {
      "kind" => kind,
      "org" => "pleme-io",
      "repo" => "r",
      "path" => "/p",
      "ref" => "x",
      "branch" => "x",
      "base" => "origin/main",
      "author" => "self",
      "age_hours" => 500
    },
    fields
  )
end

def example_failure(rule, ex, rules, ctx)
  fields = stringed(ex["item"])
  kind = fields["kind"] or first(rule["kinds"])
  got = classified(enriched(item(kind, fields), ctx), rules)["verdict"]
  if got == ex["expect"]
    []
  else
    [
      {
        "rule" => rule["name"],
        "item" => to_s(fields),
        "expected" => ex["expect"],
        "got" => got
      }
    ]
  end
end

def rule_failures(rule, rules, ctx)
  examples = rule["examples"]
  shown = flat_map(examples, (ex) -> example_failure(rule, ex, rules, ctx))
  claims = any?((ex) -> ex["expect"] == rule["verdict"], examples)
  if claims
    shown
  else
    append(
      shown,
      [
        {
          "rule" => rule["name"],
          "item" => "",
          "expected" => rule["verdict"],
          "got" => "no example reaches this rule's own verdict"
        }
      ]
    )
  end
end

@doc """
validate: Every standing rule's examples, run through the whole chain. Fails a rule whose example does not hold and any refused entry.

    validate([])["ok"] #=> true
"""
def validate(entries)
  out = rules_of(entries)
  rules = out[:rules]
  failures = flat_map(rules, (r) -> rule_failures(r, rules, ctx_fixture))
  {
    "ok" => is_empty(failures) and is_empty(out[:refusals]),
    "rules" => map((r) -> r["name"], rules),
    "off" => out[:off],
    "refusals" => out[:refusals],
    "failures" => failures
  }
end

def one(kind, fields)
  verdict_of(judge([item(kind, fields)], rules_of([]), ctx_fixture)[0])
end

test "the empty input judges to nothing and the built-in rules validate"
  assert judge([], rules_of([]), ctx_fixture) == []
  assert classes([]) == []
  assert validate([])["ok"]
  assert validate([])["failures"] == []
end

test "a rebased and landed branch is landed though ancestry says unmerged"
  assert one("branch", {"ahead" => 5, "unique" => 0, "merges" => 0}) == "landed"
  assert one("remote-branch", {"ahead" => 5, "unique" => 0, "merges" => 0}) ==
    "landed"
end

test "unique commits linger, and an unknown count is never read as landed"
  assert one("branch", {"ahead" => 56, "unique" => 56, "merges" => 0}) ==
    "lingering"
  assert one("branch", {"ahead" => 5}) == "lingering"
  assert one("branch", {"ahead" => 5, "unique" => 0}) == "lingering"
  assert one("branch", {}) == "lingering"
end

test "a branch of merges alone is not landed by the patch comparison"
  assert one("branch", {"ahead" => 2, "unique" => 0, "merges" => 2}) ==
    "lingering"
end

test "young work is active whatever it carries, and the threshold itself lingers"
  assert one("branch", {"age_hours" => 71, "ahead" => 0}) == "active"
  assert one(
    "branch",
    {"age_hours" => 72, "ahead" => 1, "unique" => 1, "merges" => 0}
  ) == "lingering"
end

test "an age is the clock against the commit, in whole hours"
  raw = item("branch", {"committed_unix" => 900 * 3600 + 1})
  assert enriched(raw, ctx_fixture)["age_hours"] == 99
end

test "another author is foreign and report-only, and a landed one is still only reported"
  row = nth(
    0,
    judge(
      [item("branch", {"author" => "someone", "ahead" => 0})],
      rules_of([]),
      ctx_fixture
    )
  )
  assert verdict_of(row) == "foreign-author"
  assert row[4]
  assert not includes(row[3], "-D")
end

test "an unread author is treated as foreign"
  assert one("branch", {"author" => nil, "ahead" => 0}) == "foreign-author"
end

def destructive?(text)
  any?(
    (w) -> includes(text, w),
    [
      "-D",
      "--delete",
      "stash drop",
      "worktree remove",
      "worktree prune",
      "update-ref -d"
    ]
  )
end

test "no protected item in any class gets a command that deletes"
  variants = [
    item("branch", {"ahead" => 0}),
    item("remote-branch", {"ahead" => 0}),
    item("stash", {"message" => "retire-spec-autostash-1"}),
    item("stash", {"message" => "mine"}),
    item("worktree", {"exists" => false}),
    item("worktree", {"exists" => true, "dirty" => false}),
    item("backup-ref", {}),
    item("default-branch", {"no_merge_base" => true})
  ]
  protected = map((v) -> assoc(v, "org", "akeylesslabs"), variants)
  rows = judge(protected, rules_of([]), ctx_fixture)
  assert size(rows) == 8
  assert every?((r) -> r[4], rows)
  assert not any?((r) -> destructive?(r[3]), rows)
  controls = judge(variants, rules_of([]), ctx_fixture)
  assert any?((r) -> destructive?(r[3]), controls)
end

test "an operator rule that suggests a delete cannot reach a protected item"
  greedy = rule(
    "greedy",
    {
      verdict: "greedy",
      kinds: ["branch"],
      severity: "warn",
      next: "git branch -D {branch}",
      examples: [example({kind: "branch"}, "greedy")]
    }
  )
  rules = rules_of([greedy])
  row = nth(
    0,
    judge([item("branch", {"org" => "akeylesslabs"})], rules, ctx_fixture)
  )
  assert verdict_of(row) == "greedy"
  assert not destructive?(row[3])
  mine = judge([item("branch", {})], rules, ctx_fixture)[0]
  assert destructive?(mine[3])
end

test "bulk sweep stashes are told from individual ones by their message"
  assert one("stash", {"message" => "On main: claude-fleet-clean-3"}) ==
    "bulk-sweep-stash"
  assert one("stash", {"message" => "On x: retire-spec-autostash-2"}) ==
    "bulk-sweep-stash"
  assert one("stash", {"message" => "WIP on main: abc"}) == "individual-stash"
  assert one("stash", {}) == "individual-stash"
end

test "worktrees, backup refs and a rewritten origin"
  assert one("worktree", {"exists" => false}) == "dead-worktree"
  assert one("worktree", {"exists" => true, "dirty" => true}) ==
    "dirty-worktree"
  assert one("worktree", {"exists" => true, "dirty" => false}) ==
    "idle-worktree"
  assert one("backup-ref", {}) == "backup-ref"
  assert one("default-branch", {"no_merge_base" => true}) ==
    "default-branch-diverged"
  assert one("default-branch", {}) == "unclassified"
end

test "off and tune reach one rule and leave the rest"
  off = {"entry" => "off", "name" => "active", "value" => {"target" => "rule"}}
  tuned = {
    "entry" => "tune",
    "name" => "active",
    "value" => {
      "target" => "rule",
      "fields" => {"scope" => {"max_age_hours" => 5}}
    }
  }
  young = [item("branch", {"age_hours" => 10, "ahead" => 0})]
  plain = judge(young, rules_of([]), ctx_fixture)
  assert verdict_of(plain[0]) == "active"
  assert verdict_of(judge(young, rules_of([off]), ctx_fixture)[0]) == "landed"
  assert verdict_of(judge(young, rules_of([tuned]), ctx_fixture)[0]) == "landed"
  assert rules_of([off])[:off] == ["active"]
end

test "a bad entry is refused alone and its siblings stand"
  bad = {"entry" => "rule", "name" => "bad", "value" => {"verdict" => "x"}}
  out = rules_of([bad])
  assert size(out[:refusals]) > 0
  assert includes(first(out[:refusals]), "bad")
  assert size(out[:rules]) == size(rules_of([])[:rules])
end

test "validate fails a rule whose example does not hold, and one with no reach"
  wrong = {
    "entry" => "rule",
    "name" => "wrong",
    "value" => {
      "verdict" => "wrong",
      "kinds" => ["stash"],
      "when" => [["message", "contains_any", ["zzz"]]],
      "severity" => "info",
      "examples" => [
        {"item" => {"kind" => "stash", "message" => "aaa"}, "expect" => "wrong"}
      ]
    }
  }
  out = validate([wrong])
  assert not out["ok"]
  assert first(out["failures"])["rule"] == "wrong"
  assert first(out["failures"])["got"] == "individual-stash"
end

test "a clause on an op that does not exist is refused"
  bad = {
    "entry" => "rule",
    "name" => "bad-op",
    "value" => {
      "verdict" => "x",
      "kinds" => ["stash"],
      "when" => [["message", "nope", 1]],
      "severity" => "info",
      "examples" => [{"item" => {}, "expect" => "x"}]
    }
  }
  assert not validate([bad])["ok"]
end

test "scope narrows by org, repo, ref glob and author"
  scoped = rule(
    "scoped",
    {
      verdict: "scoped",
      kinds: ["branch"],
      scope: {
        org: ["pleme-io"],
        repo: ["te*"],
        ref: ["wip/*"],
        author: ["self"]
      },
      severity: "info",
      examples: [
        example({kind: "branch", repo: "tend", ref: "wip/a"}, "scoped"),
        example(
          {kind: "branch", repo: "blue", ref: "wip/a", ahead: 0},
          "landed"
        )
      ]
    }
  )
  assert validate([scoped])["ok"]
end

test "classes count items and the repos that hold them, worst first"
  rows = [
    ["landed", "warn", 1, true, "a"],
    ["landed", "warn", 1, true, "a"],
    ["landed", "warn", 1, true, "b"],
    ["bulk", "info", 2, true, "a"],
    ["gone", "alert", 0, true, "c"]
  ]
  out = classes(rows)
  assert map((c) -> c["verdict"], out) == %w[gone landed bulk]
  assert out[1]["count"] == 3
  assert out[1]["repos"] == 2
end

test "the session lines are one per class, never per branch, and quiet classes stay out"
  cls = [
    {
      "verdict" => "landed",
      "severity" => "warn",
      "count" => 40,
      "repos" => 9,
      "announce" => true
    },
    {
      "verdict" => "active",
      "severity" => "info",
      "count" => 3,
      "repos" => 1,
      "announce" => false
    }
  ]
  meta = {
    "snapshot" => true,
    "stale" => false,
    "blind" => 0,
    "total" => 10,
    "age_secs" => 60
  }
  assert session_lines(cls, meta) ==
    ["tend sprawl: 40 landed in 9 repos (warn); tend sprawl --verdict landed."]
end

test "a stale snapshot and a blind repo both say so"
  meta = {
    "snapshot" => true,
    "stale" => true,
    "why" => "The daemon runs; its last cycle was throttled.",
    "blind" => 2,
    "total" => 10,
    "age_secs" => 7200
  }
  lines = session_lines([], meta)
  assert size(lines) == 2
  assert includes(first(lines), "2h old")
  assert includes(first(lines), "may be behind")
  assert includes(first(lines), "throttled")
  assert includes(lines[1], "2 repos blind of 10")
end

test "the outcome is blind when an unread repo could hold the answer"
  assert outcome(0, 0, true) == "empty"
  assert outcome(0, 1, true) == "blind"
  assert outcome(1, 1, true) == "found"
  assert outcome(0, 0, false) == "blind"
end
