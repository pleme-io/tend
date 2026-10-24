use("moji", [:includes])
use("retsu", [:first, :is_empty, :last, :size, :take_while])

def text(parts)
  join(parts, "")
end

def passes(conclusion)
  conclusion == "success"
end

def fails(conclusion)
  conclusion == "failure" or
    conclusion == "timed_out" or
    conclusion == "startup_failure"
end

def conclusive(run)
  passes(first(run)) or fails(first(run))
end

def finding(inst, state, why, since)
  {
    "instance" => get(inst, "name"),
    "state" => state,
    "detail" => why,
    "since" => since
  }
end

def red_state(age, window)
  if age > window
    "red-past-window"
  else
    "red-within-window"
  end
end

def gate_state(inst, window, now)
  runs = filter(conclusive, get(inst, "runs"))
  if is_empty(runs)
    finding(inst, "blind", "no finished run on the gate branch", "")
  elsif passes(first(first(runs)))
    finding(inst, "green", "", "")
  else
    oldest = last(take_while((r) -> fails(first(r)), runs))
    finding(
      inst,
      red_state(now - nth(1, oldest), window),
      nth(3, first(runs)),
      nth(2, oldest)
    )
  end
end

def command_state(inst)
  if get(inst, "drifted")
    finding(inst, "drifted", get(inst, "detail"), "")
  else
    finding(inst, "green", "", "")
  end
end

def instance_state(check, inst, now)
  if get(inst, "sight") == "blind"
    finding(inst, "blind", get(inst, "why"), "")
  elsif get(check, "source") == "gate"
    gate_state(inst, get(check, "window"), now)
  else
    command_state(inst)
  end
end

def alerting(state)
  state == "drifted" or state == "red-past-window"
end

def with_state(fs, pred)
  filter((f) -> pred(get(f, "state")), fs)
end

def blind_rows(check, found)
  whole = if get(check, "sight") == "blind"
    [
      {
        "instance" => get(check, "name"),
        "state" => "blind",
        "detail" => get(check, "why"),
        "since" => ""
      }
    ]
  else
    []
  end
  part = if get(check, "complete") or get(check, "sight") == "blind"
    []
  else
    [
      {
        "instance" => "(rest of the check)",
        "state" => "blind",
        "detail" => get(check, "why"),
        "since" => ""
      }
    ]
  end
  append(append(whole, part), with_state(found, (s) -> s == "blind"))
end

def verdict_of(alerts, blind, watch)
  if not is_empty(alerts)
    get(first(alerts), "state")
  elsif not is_empty(blind)
    "blind"
  elsif not is_empty(watch)
    "red-within-window"
  else
    "green"
  end
end

def judge_check(check, now)
  base = {
    "name" => get(check, "name"),
    "kind" => get(check, "kind"),
    "template" => get(check, "template")
  }
  if get(check, "enabled") == false
    assoc(
      assoc(assoc(assoc(base, "verdict", "off"), "alerts", []), "blind", []),
      "watch",
      []
    )
  else
    found = map((i) -> instance_state(check, i, now), get(check, "instances"))
    alerts = with_state(found, alerting)
    blind = blind_rows(check, found)
    watch = with_state(found, (s) -> s == "red-within-window")
    green = size(with_state(found, (s) -> s == "green"))
    out = assoc(base, "verdict", verdict_of(alerts, blind, watch))
    assoc(
      assoc(
        assoc(
          assoc(assoc(out, "alerts", alerts), "blind", blind),
          "watch",
          watch
        ),
        "green",
        green
      ),
      "instances",
      size(found)
    )
  end
end

def group_key(r)
  text([get(r, "kind"), " ", get(r, "template")])
end

def groups(results)
  reduce(
    fn(acc, r)
      k = group_key(r)
      prior = get(acc, k)
      if prior.nil?
        assoc(acc, k, [r])
      else
        assoc(acc, k, append(prior, [r]))
      end
    end,
    {},
    results
  )
end

def title_of(kind, template)
  text(["drift sentinel: ", kind, " instances of ", template])
end

def row_line(r, a)
  since = if get(a, "since") == ""
    "-"
  else
    get(a, "since")
  end
  cells = [
    get(r, "name"),
    get(a, "instance"),
    get(a, "state"),
    since,
    get(a, "detail")
  ]
  text(["| ", join(cells, " | "), " |"])
end

def alert_lines(rs)
  reduce(
    (acc, r) -> append(acc, map((a) -> row_line(r, a), get(r, "alerts"))),
    [],
    rs
  )
end

def open_body(kind, template, rs)
  head = [
    text(
      [
        "tend's drift sentinel found instances of kind `",
        kind,
        "` that left the template in `",
        template,
        "`, or a gate that stayed red past its window."
      ]
    ),
    "",
    "| Check | Instance | State | Since | Detail |",
    "|---|---|---|---|---|"
  ]
  tail = [
    "",
    "tend updates this issue on each sweep and closes it when every check of this kind is green. A blind check never closes it. The checks are `drift.checks.<name>` in tend's config."
  ]
  join(append(append(head, alert_lines(rs)), tail), "\n")
end

def close_body(kind, template)
  text(
    [
      "Every drift check of kind `",
      kind,
      "` for `",
      template,
      "` is green. tend closed this issue."
    ]
  )
end

def plan_group(rs)
  r0 = first(rs)
  kind = get(r0, "kind")
  template = get(r0, "template")
  base = {
    "kind" => kind,
    "template" => template,
    "title" => title_of(kind, template)
  }
  any_alert = not is_empty(filter((r) -> not is_empty(get(r, "alerts")), rs))
  all_green = is_empty(filter((r) -> get(r, "verdict") != "green", rs))
  if any_alert
    assoc(assoc(base, "action", "open"), "body", open_body(kind, template, rs))
  elsif all_green
    assoc(assoc(base, "action", "close"), "body", close_body(kind, template))
  else
    assoc(assoc(base, "action", "hold"), "body", "")
  end
end

def count_verdict(results, v)
  size(filter((r) -> get(r, "verdict") == v, results))
end

def overall(results)
  live = filter((r) -> get(r, "verdict") != "off", results)
  if not is_empty(filter((r) -> not is_empty(get(r, "alerts")), live))
    "alert"
  elsif not is_empty(filter((r) -> not is_empty(get(r, "blind")), live))
    "blind"
  elsif not is_empty(filter((r) -> not is_empty(get(r, "watch")), live))
    "watch"
  else
    "green"
  end
end

def judge(checks, now)
  results = map((c) -> judge_check(c, now), checks)
  {
    "verdict" => overall(results),
    "checks" => results,
    "issues" => map(
      plan_group,
      map((e) -> nth(1, e), map_entries(groups(results)))
    ),
    "alerts" => reduce((n, r) -> n + size(get(r, "alerts")), 0, results),
    "blind" => size(filter((r) -> not is_empty(get(r, "blind")), results)),
    "off" => count_verdict(results, "off")
  }
end

def hour()
  3600
end

def gate(name, runs)
  {
    "name" => name,
    "sight" => "seen",
    "why" => "",
    "drifted" => false,
    "detail" => "",
    "runs" => runs
  }
end

def tool(name, drifted, detail)
  {
    "name" => name,
    "sight" => "seen",
    "why" => "",
    "drifted" => drifted,
    "detail" => detail,
    "runs" => []
  }
end

def check(name, kind, source, instances)
  {
    "name" => name,
    "kind" => kind,
    "template" => text(["pleme-io/", kind]),
    "enabled" => true,
    "window" => 24 * hour,
    "source" => source,
    "sight" => "seen",
    "why" => "",
    "complete" => true,
    "instances" => instances
  }
end

def run(conclusion, at)
  [conclusion, at, text(["t", to_s(at)]), text(["https://example/", to_s(at)])]
end

def now()
  100 * hour
end

test "a gate red past its window alerts and names when it went red"
  red = gate(
    "pleme-io/flows",
    [
      run("failure", 90 * hour),
      run("failure", 50 * hour),
      run("success", 40 * hour)
    ]
  )
  out = judge([check("parity", "flows", "gate", [red])], now)
  r = first(get(out, "checks"))
  assert get(r, "verdict") == "red-past-window"
  assert get(first(get(r, "alerts")), "since") == "t180000"
  assert get(out, "verdict") == "alert"
  assert get(first(get(out, "issues")), "action") == "open"
end

test "a gate red within its window is watched, not alerted, and holds the issue"
  red = gate(
    "pleme-io/flows",
    [run("failure", 90 * hour), run("success", 80 * hour)]
  )
  out = judge([check("parity", "flows", "gate", [red])], now)
  r = first(get(out, "checks"))
  assert get(r, "verdict") == "red-within-window"
  assert is_empty(get(r, "alerts"))
  assert get(out, "verdict") == "watch"
  assert get(first(get(out, "issues")), "action") == "hold"
end

test "cancelled and skipped runs are not evidence either way"
  g = gate(
    "pleme-io/flows",
    [
      run("cancelled", 99 * hour),
      run("success", 98 * hour),
      run("failure", 10 * hour)
    ]
  )
  r = first(get(judge([check("parity", "flows", "gate", [g])], now), "checks"))
  assert get(r, "verdict") == "green"
end

test "a gate with no finished run is blind, never green"
  g = gate("pleme-io/flows", [run("cancelled", 99 * hour)])
  out = judge([check("parity", "flows", "gate", [g])], now)
  r = first(get(out, "checks"))
  assert get(r, "verdict") == "blind"
  assert get(out, "verdict") == "blind"
  assert get(first(get(out, "issues")), "action") == "hold"
end

test "a drifted instance alerts; the clean one is counted green"
  c = check(
    "repo",
    "repo-forge",
    "command",
    [tool("acervo", true, "2 missing"), tool("kensa", false, "")]
  )
  out = judge([c], now)
  r = first(get(out, "checks"))
  assert get(r, "verdict") == "drifted"
  assert get(r, "green") == 1
  assert get(first(get(r, "alerts")), "instance") == "acervo"
  assert includes(
    get(first(get(out, "issues")), "body"),
    "| repo | acervo | drifted | - | 2 missing |"
  )
end

test "a check that stopped early reports what it saw and is blind for the rest"
  c = assoc(
    assoc(
      check("repo", "repo-forge", "command", [tool("kensa", false, "")]),
      "complete",
      false
    ),
    "why",
    "render arnes failed"
  )
  r = first(get(judge([c], now), "checks"))
  assert get(r, "verdict") == "blind"
  assert get(first(get(r, "blind")), "detail") == "render arnes failed"
end

test "a blind check is blind and never closes its issue"
  c = assoc(
    assoc(check("skill", "skills", "command", []), "sight", "blind"),
    "why",
    "skill-lint: not found"
  )
  out = judge([c], now)
  assert get(first(get(out, "checks")), "verdict") == "blind"
  assert get(first(get(out, "issues")), "action") == "hold"
end

test "an instance that could not be read is blind"
  g = assoc(
    assoc(gate("pleme-io/flows", []), "sight", "blind"),
    "why",
    "rate limited"
  )
  r = first(get(judge([check("parity", "flows", "gate", [g])], now), "checks"))
  assert get(r, "verdict") == "blind"
end

test "green on every check of a kind closes its issue"
  g = gate(
    "pleme-io/flows",
    [run("success", 99 * hour), run("failure", 98 * hour)]
  )
  out = judge([check("parity", "flows", "gate", [g])], now)
  assert get(out, "verdict") == "green"
  assert get(first(get(out, "issues")), "action") == "close"
end

test "an off check is off and holds its issue"
  c = assoc(check("parity", "flows", "gate", []), "enabled", false)
  out = judge([c], now)
  assert get(first(get(out, "checks")), "verdict") == "off"
  assert get(out, "off") == 1
  assert get(out, "verdict") == "green"
  assert get(first(get(out, "issues")), "action") == "hold"
end

test "one issue per kind and template, however many checks feed it"
  a = check(
    "parity-a",
    "flows",
    "gate",
    [gate("x", [run("failure", 1 * hour)])]
  )
  b = check(
    "parity-b",
    "flows",
    "gate",
    [gate("y", [run("failure", 2 * hour)])]
  )
  out = judge([a, b], now)
  assert size(get(out, "issues")) == 1
  assert get(out, "alerts") == 2
end
