use("junjo", [:sort_stable_by])
use("retsu", [:first, :indexes, :is_empty, :rest, :update_at, :zip])

def bytes_of(c)
  first(c)
end

def age_of(c)
  nth(1, c)
end

def lock_held(c)
  nth(2, c)
end

def process_of(c)
  nth(3, c)
end

def max_idle(p)
  first(p)
end

def budget(p)
  nth(1, p)
end

def pressure_budget(p)
  nth(2, p)
end

def active_window(p)
  nth(3, p)
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
      if removes(nth(1, cv))
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
  keep = filter((i) -> nth(i, verdicts) == "keep", indexes(cands))
  largest = sort_stable_by((i) -> -bytes_of(nth(i, cands)), keep)
  sort_stable_by((i) -> -age_of(nth(i, cands)), largest)
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
      after - bytes_of(nth(i, cands)),
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
  after = nth(1, trimmed)
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
  assert first(out) == ["remove-idle", "keep"]
  assert nth(2, out) == 10
end

test "exactly at the idle limit is kept"
  assert first(decide([idle(10, 14)], policy, false)) == ["keep"]
end

test "over budget removes the least recently used first, and no more than needed"
  out = decide([idle(50, 2), idle(50, 9), idle(50, 5)], policy, false)
  assert first(out) == ["keep", "remove-over-budget", "keep"]
  assert nth(2, out) == 100
  assert nth(4, out) == false
end

test "idle removals count toward the budget"
  out = decide([idle(80, 30), idle(60, 2)], policy, false)
  assert first(out) == ["remove-idle", "keep"]
end

test "pressure tightens the budget"
  cands = [idle(20, 1), idle(20, 2), idle(20, 3)]
  assert first(decide(cands, policy, false)) == ["keep", "keep", "keep"]
  tight = decide(cands, policy, true)
  assert nth(3, tight) == 30
  assert first(tight) == ["keep", "remove-over-budget", "remove-over-budget"]
end

test "pressure never loosens the budget"
  loose = [14 * day, 100, 500, 3600]
  assert nth(3, decide([idle(1, 1)], loose, true)) == 100
end

test "at equal age the larger goes first"
  out = decide([idle(40, 5), idle(70, 5), idle(40, 1)], policy, false)
  assert first(out) == ["keep", "remove-over-budget", "keep"]
end

test "every in-use signal keeps a dir that is idle and over budget"
  locked = [500, 90 * day, true, "free"]
  used = [500, 90 * day, false, "used"]
  unseen = [500, 90 * day, false, "unseen"]
  recent = [500, 600, false, "free"]
  out = decide([locked, used, unseen, recent], policy, true)
  assert first(out) ==
    ["in-use-lock", "in-use-process", "in-use-unseen", "in-use-recent"]
  assert nth(4, out) == true
end

test "a future mtime reads as in use, not idle"
  assert first(decide([[10, 0, false, "free"]], policy, false)) ==
    ["in-use-recent"]
end
