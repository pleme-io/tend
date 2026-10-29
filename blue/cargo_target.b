use("junjo", [:sort_stable_by])
use("retsu", %i[first indexes is_empty rest update_at zip])

def bytes_of(c)
  first(c)
end

def age_of(c)
  c[1]
end

def lock_held(c)
  c[2]
end

def process_of(c)
  c[3]
end

def max_idle(p)
  first(p)
end

def budget(p)
  p[1]
end

def pressure_budget(p)
  p[2]
end

def active_window(p)
  p[3]
end

def removes(v)
  v == "remove-idle" or v == "remove-over-budget"
end

def effective_budget(p, pressured)
  if pressured and pressure_budget(p) < budget(p)
    pressure_budget(p)
  else
    budget(p)
  end
end

def verdict_of(p, c)
  if lock_held(c)
    "in-use-lock"
  elsif process_of(c) == "used"
    "in-use-process"
  elsif process_of(c) == "unseen"
    "in-use-unseen"
  elsif age_of(c) < active_window(p)
    "in-use-recent"
  elsif age_of(c) > max_idle(p)
    "remove-idle"
  else
    "keep"
  end
end

def kept_bytes(cands, verdicts)
  reduce(
    fn(acc, cv)
      if removes(cv[1])
        acc
      else
        acc + bytes_of(first(cv))
      end
    end,
    0,
    zip(cands, verdicts)
  )
end

def removal_order(cands, verdicts)
  keep = filter((i) -> verdicts[i] == "keep", indexes(cands))
  largest = sort_stable_by((i) -> -bytes_of(cands[i]), keep)
  sort_stable_by((i) -> -age_of(cands[i]), largest)
end

def trim(cands, verdicts, order, after, limit)
  if after <= limit or is_empty(order)
    [verdicts, after]
  else
    i = first(order)
    trim(
      cands,
      update_at(verdicts, i, "remove-over-budget"),
      rest(order),
      after - bytes_of(cands[i]),
      limit
    )
  end
end

def decide(cands, p, pressured)
  verdicts = map((c) -> verdict_of(p, c), cands)
  total = reduce((acc, c) -> acc + bytes_of(c), 0, cands)
  limit = effective_budget(p, pressured)
  trimmed = trim(
    cands,
    verdicts,
    removal_order(cands, verdicts),
    kept_bytes(cands, verdicts),
    limit
  )
  after = trimmed[1]
  [first(trimmed), total, after, limit, after > limit]
end

def day()
  86400
end

def policy()
  [14 * day, 100, 30, 3600]
end

def idle(bytes, days)
  [bytes, days * day, false, "free"]
end

test "idle past the limit is removed, fresh is kept"
  out = decide([idle(10, 30), idle(10, 3)], policy, false)
  assert first(out) == %w[remove-idle keep]
  assert out[2] == 10
end

test "exactly at the idle limit is kept"
  assert first(decide([idle(10, 14)], policy, false)) == ["keep"]
end

test "over budget removes the least recently used first, and no more than needed"
  out = decide([idle(50, 2), idle(50, 9), idle(50, 5)], policy, false)
  assert first(out) == %w[keep remove-over-budget keep]
  assert out[2] == 100
  assert out[4] == false
end

test "idle removals count toward the budget"
  out = decide([idle(80, 30), idle(60, 2)], policy, false)
  assert first(out) == %w[remove-idle keep]
end

test "pressure tightens the budget"
  cands = [idle(20, 1), idle(20, 2), idle(20, 3)]
  assert first(decide(cands, policy, false)) == %w[keep keep keep]
  tight = decide(cands, policy, true)
  assert tight[3] == 30
  assert first(tight) == %w[keep remove-over-budget remove-over-budget]
end

test "pressure never loosens the budget"
  loose = [14 * day, 100, 500, 3600]
  assert decide([idle(1, 1)], loose, true)[3] == 100
end

test "at equal age the larger goes first"
  out = decide([idle(40, 5), idle(70, 5), idle(40, 1)], policy, false)
  assert first(out) == %w[keep remove-over-budget keep]
end

test "every in-use signal keeps a dir that is idle and over budget"
  locked = [500, 90 * day, true, "free"]
  used = [500, 90 * day, false, "used"]
  unseen = [500, 90 * day, false, "unseen"]
  recent = [500, 600, false, "free"]
  out = decide([locked, used, unseen, recent], policy, true)
  assert first(out) ==
    %w[in-use-lock in-use-process in-use-unseen in-use-recent]
  assert out[4] == true
end

test "a future mtime reads as in use, not idle"
  assert first(decide([[10, 0, false, "free"]], policy, false)) ==
    ["in-use-recent"]
end
