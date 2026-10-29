use("junjo", [:sort_stable_by])
use("retsu", %i[first indexes])

def age_of(b)
  first(b)
end

def ahead_of(b)
  b[1]
end

def verdict_of(threshold, b)
  if age_of(b) < threshold
    "active"
  elsif ahead_of(b) == 0
    "landed"
  else
    "lingering"
  end
end

def decide(branches, threshold)
  verdicts = map((b) -> verdict_of(threshold, b), branches)
  order = sort_stable_by((i) -> -age_of(branches[i]), indexes(branches))
  [verdicts, order]
end

def hour()
  3600
end

test "younger than the threshold is active, whatever it carries"
  out = decide([[3 * hour, 4], [1 * hour, 0]], 24 * hour)
  assert first(out) == %w[active active]
end

test "past the threshold with unlanded commits it lingers"
  assert first(decide([[30 * hour, 2]], 24 * hour)) == ["lingering"]
end

test "past the threshold with nothing ahead of main it is landed"
  assert first(decide([[30 * hour, 0]], 24 * hour)) == ["landed"]
end

test "an unknown ahead count is never read as landed"
  assert first(decide([[30 * hour, -1]], 24 * hour)) == ["lingering"]
end

test "exactly at the threshold it lingers"
  assert first(decide([[24 * hour, 1]], 24 * hour)) == ["lingering"]
end

test "the oldest comes first, ties keep their order"
  out = decide(
    [[2 * hour, 1], [90 * hour, 1], [2 * hour, 1], [40 * hour, 1]],
    24 * hour
  )
  assert out[1] == [1, 3, 0, 2]
end
