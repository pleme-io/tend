@moduledoc """
scratchpad: the judgement over each agent session's scratchpad under the Claude temp root. Report only: a verdict and the fix as text; nothing here deletes.

A scratchpad past the size threshold is `scratchpad-oversize`. Two or more checkouts of one repo in one scratchpad, each holding its own cargo target, is `scratchpad-duplicate-targets`: every checkout builds the whole dependency graph again, and the fix is one shared CARGO_TARGET_DIR.

    scratchpad_verdicts([], {"threshold" => 1, "duplicates" => true}) #=> []
"""

use("retsu", %i[first flat_map size])

use("shuugou", %i[group_by unique])

def gib()
  1024 * 1024 * 1024
end

@doc """
gib_words: Bytes as GiB with one decimal.

    gib_words(3 * 1024 * 1024 * 1024) #=> "3.0 GiB"
    gib_words(1610612736) #=> "1.5 GiB"
"""
def gib_words(b)
  tenths = floor(b * 10 / gib)
  "#{floor(tenths / 10)}.#{tenths - floor(tenths / 10) * 10} GiB"
end

def bytes_of_all(rows)
  reduce((acc, r) -> acc + r["bytes"], 0, rows)
end

def oversize(pad, settings)
  t = settings["threshold"]
  if t > 0 and pad["bytes"] >= t
    [
      {
        "verdict" => "scratchpad-oversize",
        "pad" => pad["path"],
        "bytes" => pad["bytes"],
        "threshold" => t,
        "targets" => map((x) -> x["path"], pad["targets"])
      }
    ]
  else
    []
  end
end

def duplicate(pad, group)
  rows = group[1]
  checkouts = unique(map((r) -> r["checkout"], rows))
  if size(checkouts) >= 2
    [
      {
        "verdict" => "scratchpad-duplicate-targets",
        "pad" => pad["path"],
        "repo" => first(group),
        "checkouts" => checkouts,
        "targets" => map((r) -> r["path"], rows),
        "bytes" => bytes_of_all(rows)
      }
    ]
  else
    []
  end
end

def duplicates(pad, settings)
  if settings["duplicates"]
    owned = filter((t) -> not t["repo"].nil?, pad["targets"])
    flat_map(group_by(owned, (t) -> t["repo"]), (g) -> duplicate(pad, g))
  else
    []
  end
end

@doc """
scratchpad_verdicts: Per scratchpad, the verdicts it earns: oversize first, then one duplicate-targets per repo checked out twice or more with a target each.

    scratchpad_verdicts([{"path" => "/p", "bytes" => 9, "targets" => []}], {"threshold" => 5, "duplicates" => true})[0]["verdict"] #=> "scratchpad-oversize"
    scratchpad_verdicts([{"path" => "/p", "bytes" => 9, "targets" => []}], {"threshold" => 0, "duplicates" => true}) #=> []
"""
def scratchpad_verdicts(pads, settings)
  flat_map(pads, (p) -> append(oversize(p, settings), duplicates(p, settings)))
end

@doc """
line: One sentence per verdict, with the fix.

    line({"verdict" => "scratchpad-duplicate-targets", "pad" => "/s", "repo" => "pleme-io/tend", "checkouts" => ["/s/a", "/s/b"], "bytes" => 2147483648}) #=> "tend scratchpad: 🔴 scratchpad-duplicate-targets pleme-io/tend: 2 checkouts in /s each hold their own target (2.0 GiB). Share one CARGO_TARGET_DIR: export CARGO_TARGET_DIR=/s/cargo-target for every checkout, then delete the per-checkout target/ dirs."
"""
def line(v)
  if v["verdict"] == "scratchpad-oversize"
    "tend scratchpad: 🔴 scratchpad-oversize #{v["pad"]} holds #{gib_words(v["bytes"])} (threshold #{gib_words(v["threshold"])}), #{size(v["targets"])} cargo target(s) inside. Share one CARGO_TARGET_DIR across its checkouts and delete what the session no longer needs."
  else
    "tend scratchpad: 🔴 scratchpad-duplicate-targets #{v["repo"]}: #{size(v["checkouts"])} checkouts in #{v["pad"]} each hold their own target (#{gib_words(v["bytes"])}). Share one CARGO_TARGET_DIR: export CARGO_TARGET_DIR=#{v["pad"]}/cargo-target for every checkout, then delete the per-checkout target/ dirs."
  end
end

def lines(verdicts)
  map(line, verdicts)
end

test "a pad at the threshold is oversize, one below is not"
  pad = {"path" => "/p", "bytes" => 10, "targets" => []}
  assert size(
    scratchpad_verdicts([pad], {"threshold" => 10, "duplicates" => true})
  ) == 1
  assert size(
    scratchpad_verdicts([pad], {"threshold" => 11, "duplicates" => true})
  ) == 0
end

test "two checkouts of one repo with a target each are duplicates"
  ts = [
    {
      "path" => "/p/a/target",
      "bytes" => 4,
      "checkout" => "/p/a",
      "repo" => "o/r"
    },
    {
      "path" => "/p/b/target",
      "bytes" => 6,
      "checkout" => "/p/b",
      "repo" => "o/r"
    }
  ]
  out = scratchpad_verdicts(
    [{"path" => "/p", "bytes" => 10, "targets" => ts}],
    {"threshold" => 0, "duplicates" => true}
  )
  assert out[0]["verdict"] == "scratchpad-duplicate-targets"
  assert out[0]["bytes"] == 10
end

test "two targets in one checkout, or a shared target dir, are not duplicates"
  ts = [
    {
      "path" => "/p/a/target",
      "bytes" => 4,
      "checkout" => "/p/a",
      "repo" => "o/r"
    },
    {
      "path" => "/p/a/sub/target",
      "bytes" => 6,
      "checkout" => "/p/a",
      "repo" => "o/r"
    },
    {
      "path" => "/p/cargo-target",
      "bytes" => 6,
      "checkout" => nil,
      "repo" => nil
    }
  ]
  assert scratchpad_verdicts(
    [{"path" => "/p", "bytes" => 16, "targets" => ts}],
    {"threshold" => 0, "duplicates" => true}
  ) == []
end

test "duplicates off reports none"
  ts = [
    {
      "path" => "/p/a/target",
      "bytes" => 4,
      "checkout" => "/p/a",
      "repo" => "o/r"
    },
    {
      "path" => "/p/b/target",
      "bytes" => 6,
      "checkout" => "/p/b",
      "repo" => "o/r"
    }
  ]
  assert scratchpad_verdicts(
    [{"path" => "/p", "bytes" => 10, "targets" => ts}],
    {"threshold" => 0, "duplicates" => false}
  ) == []
end
