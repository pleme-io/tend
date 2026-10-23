use("junjo", [:sort, :sort_stable_by])
use("retsu", [:first, :flat_map, :is_empty, :rest, :size])

def edge_repo(e)
  first(e)
end

def edge_dep(e)
  nth(1, e)
end

def pin_repo(p)
  first(p)
end

def pin_item(p)
  nth(1, p)
end

def pin_version(p)
  nth(2, p)
end

def lookup(m, k, fallback)
  v = get(m, k)
  if v == nil
    fallback
  else
    v
  end
end

def mark(set, k)
  assoc(set, k, true)
end

def has(set, k)
  get(set, k) == true
end

def members(set)
  map(first, map_entries(set))
end

def grow(m, k, v)
  assoc(m, k, cons(v, lookup(m, k, [])))
end

def down_map(edges)
  reduce((m, e) -> grow(m, edge_repo(e), edge_dep(e)), {}, edges)
end

def up_map(edges)
  reduce((m, e) -> grow(m, edge_dep(e), edge_repo(e)), {}, edges)
end

def spread(down, up, frontier, seen)
  if is_empty(frontier)
    seen
  else
    n = first(frontier)
    fresh = filter(
      (x) -> has(seen, x) == false,
      append(lookup(down, n, []), lookup(up, n, []))
    )
    spread(down, up, append(fresh, rest(frontier)), reduce(mark, seen, fresh))
  end
end

def tree_ids(down, up, ns)
  reduce(
    fn(ids, n)
      if get(ids, n) == nil
        reduce(
          (acc, k) -> assoc(acc, k, n),
          ids,
          members(spread(down, up, [n], mark({}, n)))
        )
      else
        ids
      end
    end,
    {},
    ns
  )
end

def walk(down, remaining, done, waves)
  if is_empty(remaining)
    [waves, []]
  else
    ready = filter(
      fn(n)
        size(filter((d) -> has(done, d) == false, lookup(down, n, []))) == 0
      end,
      remaining
    )
    if is_empty(ready)
      [waves, remaining]
    else
      readyset = reduce(mark, {}, ready)
      walk(
        down,
        filter((n) -> has(readyset, n) == false, remaining),
        reduce(mark, done, ready),
        append(waves, [ready])
      )
    end
  end
end

def wave_index(waves, i, acc)
  if is_empty(waves)
    acc
  else
    wave_index(
      rest(waves),
      i + 1,
      reduce((m, n) -> assoc(m, n, i), acc, first(waves))
    )
  end
end

def ordered_edge(index, e)
  r = get(index, edge_repo(e))
  d = get(index, edge_dep(e))
  if r == nil or d == nil
    true
  else
    d < r
  end
end

def is_topological(edges, index)
  size(filter((e) -> ordered_edge(index, e) == false, edges)) == 0
end

def covers(ns, index)
  size(filter((n) -> get(index, n) == nil, ns)) == 0
end

def tally(versions)
  reduce((m, v) -> assoc(m, v, lookup(m, v, 0) + 1), {}, versions)
end

def distinct(versions)
  sort(members(reduce(mark, {}, versions)))
end

def target_of(versions)
  counts = tally(versions)
  reduce(
    fn(best, v)
      if get(counts, v) >= get(counts, best)
        v
      else
        best
      end
    end,
    first(distinct(versions)),
    distinct(versions)
  )
end

def by_key(key, xs)
  reduce((m, x) -> grow(m, key(x), x), {}, xs)
end

def skew_in(index, item, group)
  versions = map(pin_version, group)
  if size(distinct(versions)) < 2
    []
  else
    target = target_of(versions)
    [
      [
        item,
        target,
        map(
          fn(v)
            [v, sort(map(pin_repo, filter((p) -> pin_version(p) == v, group)))]
          end,
          distinct(versions)
        ),
        sort_stable_by(
          (m) -> first(m),
          map(
            fn(p)
              [
                get(index, pin_repo(p)),
                pin_repo(p),
                item,
                pin_version(p),
                target
              ]
            end,
            filter((p) -> pin_version(p) != target, group)
          )
        )
      ]
    ]
  end
end

def skews_of(index, ids, pins)
  usable = filter(
    (p) -> get(ids, pin_repo(p)) != nil and get(index, pin_repo(p)) != nil,
    pins
  )
  per_item = by_key(pin_item, usable)
  flat_map(
    fn(item)
      flat_map(
        fn(id)
          skew_in(
            index,
            item,
            filter((p) -> get(ids, pin_repo(p)) == id, get(per_item, item))
          )
        end,
        sort(
          members(
            reduce(
              mark,
              {},
              map((p) -> get(ids, pin_repo(p)), get(per_item, item))
            )
          )
        )
      )
    end,
    sort(members(per_item))
  )
end

def propose(ns, edges, pins)
  down = down_map(edges)
  walked = walk(down, ns, {}, [])
  waves = first(walked)
  stuck = nth(1, walked)
  index = wave_index(waves, 0, {})
  placed = filter((n) -> get(index, n) != nil, ns)
  free = filter((e) -> get(index, edge_repo(e)) != nil, edges)
  if is_topological(free, index) and covers(placed, index)
    ids = tree_ids(down_map(free), up_map(free), placed)
    found = skews_of(index, ids, pins)
    [
      "ok",
      waves,
      map((s) -> [first(s), nth(1, s), nth(2, s)], found),
      sort_stable_by((m) -> first(m), flat_map((s) -> nth(3, s), found)),
      stuck
    ]
  else
    ["refused", "order"]
  end
end

test "dependencies come before dependents, wave by wave"
  edges = [["app", "lib"], ["lib", "base"], ["tool", "base"]]
  ns = ["app", "base", "lib", "tool"]
  out = propose(ns, edges, [])
  assert nth(1, out) == [["base"], ["lib", "tool"], ["app"]]
  assert nth(4, out) == []
end

test "a cycle and what hangs off it are named, other trees stand"
  edges = [["a", "b"], ["b", "c"], ["c", "a"], ["d", "a"], ["x", "y"]]
  ns = ["a", "b", "c", "d", "x", "y"]
  out = propose(ns, edges, [])
  assert nth(1, out) == [["y"], ["x"]]
  assert nth(4, out) == ["a", "b", "c", "d"]
end

test "a wrong order is caught by the checker"
  edges = [["app", "lib"]]
  assert is_topological(edges, wave_index([["app"], ["lib"]], 0, {})) == false
  assert is_topological(edges, wave_index([["lib"], ["app"]], 0, {})) == true
  assert covers(["app", "lib"], wave_index([["lib"]], 0, {})) == false
end

test "trees are the connected parts of the dependency graph"
  edges = [["app", "lib"], ["x", "y"]]
  ids = tree_ids(down_map(edges), up_map(edges), ["app", "lib", "x", "y"])
  assert get(ids, "app") == "app"
  assert get(ids, "lib") == "app"
  assert get(ids, "x") == "x"
  assert get(ids, "y") == "x"
end

test "a pin that differs inside one tree is proposed for alignment, dependency first"
  edges = [["app", "lib"], ["lib", "base"]]
  pins = [
    ["lib", "flake:base", "r1"],
    ["app", "flake:base", "r2"],
    ["app", "crate:kit", "k1"],
    ["lib", "crate:kit", "k1"]
  ]
  out = propose(["app", "base", "lib"], edges, pins)
  assert first(out) == "ok"
  assert nth(3, out) == [[1, "lib", "flake:base", "r1", "r2"]]
end

test "the pin most repos hold is the target, and the highest breaks a tie"
  edges = [["a", "root"], ["b", "root"], ["c", "root"]]
  ns = ["a", "b", "c", "root"]
  majority = [["a", "i", "x"], ["b", "i", "y"], ["c", "i", "y"]]
  assert nth(1, first(nth(2, propose(ns, edges, majority)))) == "y"
  tie = [["a", "i", "x"], ["b", "i", "y"]]
  assert nth(1, first(nth(2, propose(ns, edges, tie)))) == "y"
end

test "pins in different trees never skew each other"
  edges = [["a", "lib1"], ["b", "lib2"]]
  pins = [["a", "i", "x"], ["b", "i", "y"]]
  out = propose(["a", "b", "lib1", "lib2"], edges, pins)
  assert nth(2, out) == []
  assert nth(3, out) == []
end

test "equal pins propose nothing"
  edges = [["a", "lib"]]
  pins = [["a", "i", "x"], ["lib", "i", "x"]]
  assert nth(3, propose(["a", "lib"], edges, pins)) == []
end

test "a skew in a healthy tree is proposed while another tree is cyclic"
  edges = [["a", "b"], ["b", "a"], ["app", "lib"]]
  pins = [["app", "i", "x"], ["lib", "i", "y"]]
  out = propose(["a", "app", "b", "lib"], edges, pins)
  assert nth(3, out) == [[1, "app", "i", "x", "y"]]
  assert nth(4, out) == ["a", "b"]
end
