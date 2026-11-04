use("retsu", %i[first flat_map indexes zip])

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
  rows = zip(indexes(cands), zip(cands, verdicts))
  keep = filter((r) -> r[1][1] == "keep", rows)
  largest = sort_keyed((r) -> -bytes_of(first(r[1])), keep)
  map(
    (r) -> [first(r), first(r[1])],
    sort_keyed((r) -> -age_of(first(r[1])), largest)
  )
end

def trim_one(acc, row, limit)
  if acc[1] <= limit
    acc
  else
    [assoc(first(acc), first(row), true), acc[1] - bytes_of(row[1])]
  end
end

def trim(verdicts, order, after, limit)
  trimmed = reduce((acc, row) -> trim_one(acc, row, limit), [{}, after], order)
  over = first(trimmed)
  final = map(
    fn(p)
      if get(over, first(p)).nil?
        p[1]
      else
        "remove-over-budget"
      end
    end,
    zip(indexes(verdicts), verdicts)
  )
  [final, trimmed[1]]
end

def decide(cands, p, pressured)
  verdicts = map((c) -> verdict_of(p, c), cands)
  total = reduce((acc, c) -> acc + bytes_of(c), 0, cands)
  limit = effective_budget(p, pressured)
  trimmed = trim(
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

@doc """
goal_of: The disk goal's state and size from a disk reading. Unread is blind, emergency pressure wins, a percent outside 1 to 99 is refused alone and acts as no goal.

    goal_of(true, 30, 100, 20, false) #=> ["met", 20]
    goal_of(true, 10, 100, 20, false) #=> ["below", 20]
    goal_of(true, 10, 100, 0, false) #=> ["off", 0]
"""
def goal_of(read, free, total, pct, pressured)
  valid = pct >= 1 and pct <= 99
  goal = if valid
    floor(total * pct / 100)
  else
    0
  end
  state = if not read
    "blind"
  elsif pressured
    "pressure"
  elsif pct == 0
    "off"
  elsif not valid
    "refused"
  elsif free < goal
    "below"
  else
    "met"
  end
  [state, goal]
end

@doc """
tightens: Whether a goal state calls for the tighter budget and for eviction.

    tightens("below") #=> true
    tightens("met") #=> false
"""
def tightens(state)
  state == "below" or state == "pressure"
end

@doc """
goal_sweep_due: Below the goal the sweep runs on the short clock. A sweep that freed nothing waits for the long clock, since measuring is the cost and the next short tick would find the same tree.

    goal_sweep_due("below", 1, 300, 300, 3600) #=> true
    goal_sweep_due("below", 0, 300, 300, 3600) #=> false
    goal_sweep_due("met", 1, 9000, 300, 3600) #=> false
"""
def goal_sweep_due(state, last_freed, elapsed, short, long)
  if state != "below"
    false
  elsif last_freed == 0
    elapsed >= long
  else
    elapsed >= short
  end
end

@doc """
need_bytes: How many bytes eviction should free. Zero when the state does not tighten; -1 when pressure leaves only the budgets to bound it.

    need_bytes("below", 10, 30) #=> 20
    need_bytes("met", 40, 30) #=> 0
    need_bytes("pressure", 10, 0) #=> -1
"""
def need_bytes(state, free, goal)
  if not tightens(state)
    0
  elsif goal > free
    goal - free
  else
    -1
  end
end

def child_key(i, j)
  i * 1_000_000 + j
end

def entry_rows(i, cands, verdicts)
  pairs = zip(indexes(cands), zip(cands, verdicts))
  picked = filter((p) -> removes(p[1][1]), pairs)
  map((p) -> [child_key(i, first(p)), first(p[1])], picked)
end

def take(acc, row, need)
  if need == -1 or first(acc) < need
    [first(acc) + bytes_of(row[1]), assoc(acc[1], first(row), true)]
  else
    acc
  end
end

def final_verdicts(i, verdicts, taken)
  map(
    fn(p)
      if removes(p[1]) and get(taken, child_key(i, first(p))).nil?
        "keep"
      else
        p[1]
      end
    end,
    zip(indexes(verdicts), verdicts)
  )
end

@doc """
plan_caches: One verdict per child per entry. Each entry decides alone, as a target dir does; then the removals run oldest first across every entry and stop once the goal is reached.

    plan_caches([[[[10, 2000, false, "free"]], [100, 5, 5, 60]]], "below", 0, 5) #=> [["remove-idle"]]
    plan_caches([[[[10, 2000, false, "free"]], [100, 5, 5, 60]]], "met", 50, 5) #=> [["keep"]]
"""
def plan_caches(entries, state, free, goal)
  decided = map((e) -> first(decide(first(e), e[1], false)), entries)
  rows = flat_map(
    zip(indexes(entries), zip(entries, decided)),
    (t) -> entry_rows(first(t), first(first(t[1])), t[1][1])
  )
  largest = sort_keyed((r) -> -bytes_of(r[1]), rows)
  oldest = sort_keyed((r) -> -age_of(r[1]), largest)
  need = need_bytes(state, free, goal)
  taken = reduce((acc, row) -> take(acc, row, need), [0, {}], oldest)[1]
  map(
    (t) -> final_verdicts(first(t), t[1], taken),
    zip(indexes(decided), decided)
  )
end

def cache_policy()
  [14 * day, 30, 30, 3600]
end

test "the goal has one state per reading"
  assert goal_of(false, 0, 0, 20, false) == ["blind", 0]
  assert goal_of(true, 1, 100, 20, true) == ["pressure", 20]
  assert goal_of(true, 1, 100, 0, false) == ["off", 0]
  assert goal_of(true, 1, 100, 100, false) == ["refused", 0]
  assert goal_of(true, 19, 100, 20, false) == ["below", 20]
  assert goal_of(true, 20, 100, 20, false) == ["met", 20]
end

test "the goal is a whole number of bytes on a real disk"
  total = 926 * 1024 * 1024 * 1024
  out = goal_of(true, 157 * 1024 * 1024 * 1024, total, 20, false)
  assert first(out) == "below"
  assert out[1] == floor(total * 20 / 100)
end

test "only below and pressure tighten"
  assert map(tightens, %w[off refused blind met below pressure]) ==
    [false, false, false, false, true, true]
end

test "a goal sweep runs on the short clock until one frees nothing"
  assert goal_sweep_due("below", -1, 0, 300, 3600) == false
  assert goal_sweep_due("below", -1, 300, 300, 3600) == true
  assert goal_sweep_due("below", 5, 299, 300, 3600) == false
  assert goal_sweep_due("below", 0, 3599, 300, 3600) == false
  assert goal_sweep_due("below", 0, 3600, 300, 3600) == true
  assert goal_sweep_due("off", 5, 9000, 300, 3600) == false
  assert goal_sweep_due("pressure", 5, 9000, 300, 3600) == false
end

test "nothing is evicted when the state does not tighten"
  e = [[idle(50, 90)], [14 * day, 0, 0, 3600]]
  assert map((s) -> plan_caches([e], s, 10, 20), %w[off refused blind met]) ==
    [[["keep"]], [["keep"]], [["keep"]], [["keep"]]]
end

test "eviction takes the oldest across entries and stops at the goal"
  a = [[idle(10, 20), idle(10, 40)], cache_policy]
  b = [[idle(10, 30)], cache_policy]
  assert plan_caches([a, b], "below", 0, 15) ==
    [%w[keep remove-idle], ["remove-idle"]]
  assert plan_caches([a, b], "below", 0, 10) == [%w[keep remove-idle], ["keep"]]
end

test "each entry trims to its own budget, oldest first"
  e = [[idle(20, 1), idle(20, 3), idle(20, 2)], cache_policy]
  assert plan_caches([e], "pressure", 0, 0) ==
    [%w[keep remove-over-budget remove-over-budget]]
end

test "an entry under budget and not idle gives nothing up"
  e = [[idle(5, 1)], cache_policy]
  assert plan_caches([e], "below", 0, 900) == [["keep"]]
end

test "every in-use signal keeps a cache child"
  rows = [
    [50, 90 * day, false, "used"],
    [50, 90 * day, false, "unseen"],
    [50, 600, false, "free"]
  ]
  assert plan_caches([[rows, cache_policy]], "pressure", 0, 0) ==
    [%w[in-use-process in-use-unseen in-use-recent]]
end
