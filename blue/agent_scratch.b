@moduledoc """
agent_scratch: the lifecycle of agent scratch directories on a node. A directory under the managed root that carries a valid `.tend-scratch` marker is reclaimed once its declared ttl has run out, or once nothing has been written in it past the idle limit, and only when no live process holds it. Disk pressure shortens the ttl by `pressure_ttl_pct`, never below `floor_hours`. A directory without a marker, or one outside the root, is debris: reported with a command as text, never deleted.

The class's configuration is declared here too: `defaults()` is the whole of it, and a node's own `agent-scratch.b` lays fields over it, each field checked alone.

    verdicts([cand(10, 9, 9 * hour, 3 * hour, "free")], settings({})["settings"], false) #=> ["remove-expired"]
"""

use("kousei", %i[field flag int_in list_of loosened record_whys text])

use("moji", [:glob_match?])

use("retsu", [:size])

def hour()
  3600
end

def gib()
  1024 * 1024 * 1024
end

@doc """
defaults: The class's configuration, every field. `enabled` false configures the class off: nothing is measured, nothing reclaimed, nothing reported.

    defaults["root"] #=> "/var/tmp/agent-scratch"
"""
def defaults()
  {
    "enabled" => true,
    "root" => "/var/tmp/agent-scratch",
    "default_ttl_hours" => 72,
    "max_idle_hours" => 168,
    "active_minutes" => 30,
    "pressure_ttl_pct" => 25,
    "floor_hours" => 6,
    "watch_paths" => %w[/tmp ~],
    "patterns" => %w[
      engenho-mountns-*
      cg-repro*
      cg-roots*
      agent-scratch-*
      *-agent-scratch
    ],
    "debris_alert_gib" => 1,
    "interval_minutes" => 15
  }
end

def table()
  [
    field("enabled", flag),
    field("root", text),
    field("default_ttl_hours", int_in(1, 8760)),
    field("max_idle_hours", int_in(1, 8760)),
    field("active_minutes", int_in(0, 1440)),
    field("pressure_ttl_pct", int_in(1, 100)),
    field("floor_hours", int_in(0, 8760)),
    field("watch_paths", list_of(text)),
    field("patterns", list_of(text)),
    field("debris_alert_gib", int_in(0, 100_000)),
    field("interval_minutes", int_in(1, 1440))
  ]
end

def laid(acc, pair)
  whys = record_whys(loosened(table), {pair[0] => pair[1]})
  if size(whys) == 0
    assoc(acc, "settings", assoc(acc["settings"], pair[0], pair[1]))
  else
    assoc(acc, "refusals", append(acc["refusals"], whys))
  end
end

@doc """
settings: The node's fields laid over `defaults()`. A field that is unknown or ill-typed is refused alone and its default stands.

    settings({"floor_hours" => 2})["settings"]["floor_hours"] #=> 2
    settings({"floor_hours" => "two"})["refusals"] #=> ["floor_hours: two is not a whole number in 0..8760"]
"""
def settings(overrides)
  reduce(
    laid,
    {"settings" => defaults, "refusals" => []},
    map_entries(overrides)
  )
end

def marker_table()
  [
    field("owner", text),
    field("created_unix", int_in(0, 100_000_000_000)),
    field("ttl_hours", int_in(1, 8760)),
    field("purpose", text)
  ]
end

def missing(m)
  filter((k) -> get(m, k).nil?, %w[owner created_unix ttl_hours])
end

@doc """
marker_whys: Why a marker's value is not a marker: each missing or ill-typed field, and each field it does not know. Empty means valid.

    marker_whys({"owner" => "s1", "created_unix" => 1, "ttl_hours" => 3}) #=> []
    marker_whys({"owner" => "s1", "created_unix" => 1}) #=> ["ttl_hours is required"]
"""
def marker_whys(m)
  if not map?(m)
    ["the marker is not a record"]
  else
    append(
      map((k) -> "#{k} is required", missing(m)),
      record_whys(loosened(marker_table), m)
    )
  end
end

@doc """
effective_ttl: The ttl a directory is held to. Under disk pressure it is cut to `pressure_ttl_pct` percent, but never below `floor_hours`, and the cut never lengthens a ttl already under the floor.

    effective_ttl(100 * hour, {"pressure_ttl_pct" => 25, "floor_hours" => 6}, true) #=> 25 * hour
    effective_ttl(8 * hour, {"pressure_ttl_pct" => 25, "floor_hours" => 6}, true) #=> 6 * hour
    effective_ttl(2 * hour, {"pressure_ttl_pct" => 25, "floor_hours" => 6}, true) #=> 2 * hour
    effective_ttl(8 * hour, {"pressure_ttl_pct" => 25, "floor_hours" => 6}, false) #=> 8 * hour
"""
def effective_ttl(ttl, s, pressured)
  floor_s = s["floor_hours"] * hour
  cut = floor(ttl * s["pressure_ttl_pct"] / 100)
  if not pressured or ttl <= floor_s
    ttl
  elsif cut < floor_s
    floor_s
  else
    cut
  end
end

@doc """
verdict_of: One marked directory's verdict. A process holding it, an unreadable process table, or a write inside the active window keeps it whatever its ttl says.

    verdict_of({"bytes" => 1, "idle_secs" => 9 * hour, "age_secs" => 9 * hour, "ttl_secs" => 3 * hour, "process" => "used"}, settings({})["settings"], false) #=> "in-use-process"
"""
def verdict_of(c, s, pressured)
  if c["process"] == "used"
    "in-use-process"
  elsif c["process"] == "unseen"
    "in-use-unseen"
  elsif c["idle_secs"] < s["active_minutes"] * 60
    "in-use-recent"
  elsif c["age_secs"] > effective_ttl(c["ttl_secs"], s, pressured)
    "remove-expired"
  elsif c["idle_secs"] > s["max_idle_hours"] * hour
    "remove-idle"
  else
    "keep"
  end
end

def verdicts(cands, s, pressured)
  map((c) -> verdict_of(c, s, pressured), cands)
end

def cand(bytes, idle_h, age_s, ttl_s, process)
  {
    "bytes" => bytes,
    "idle_secs" => idle_h * hour,
    "age_secs" => age_s,
    "ttl_secs" => ttl_s,
    "process" => process
  }
end

@doc """
debris_names: The names under a watch path that look like agent scratch: those any configured pattern matches.

    debris_names(%w[cg-repro notes engenho-mountns-7], {"patterns" => %w[cg-repro* engenho-mountns-*]}) #=> ["cg-repro", "engenho-mountns-7"]
"""
def debris_names(names, s)
  filter((n) -> any?((p) -> glob_match?(p, n), s["patterns"]), names)
end

def gib_words(b)
  tenths = floor(b * 10 / gib)
  "#{floor(tenths / 10)}.#{tenths - floor(tenths / 10) * 10} GiB"
end

def hours_words(secs)
  "#{floor(secs / hour)}h"
end

@doc """
debris_line: One sentence per piece of debris, with the command for the operator as text. Nothing runs it.

    debris_line({"path" => "/tmp/cg-repro", "kind" => "unmarked", "bytes" => 1073741824, "idle_secs" => 7200, "why" => ""}) #=> "tend agent-scratch: ⚠️ /tmp/cg-repro is agent scratch with no .tend-scratch marker (1.0 GiB, idle 2h); never reclaimed. Inspect it, then: rm -rf -- '/tmp/cg-repro'"
"""
def debris_line(d)
  head = "tend agent-scratch: ⚠️ #{d["path"]}"
  size_words = "#{gib_words(d["bytes"])}, idle #{hours_words(d["idle_secs"])}"
  tail = "never reclaimed. Inspect it, then: rm -rf -- '#{d["path"]}'"
  if d["kind"] == "marked-outside-root"
    "#{head} carries a marker but sits outside the managed root (#{size_words}); #{tail}"
  elsif d["kind"] == "bad-marker"
    "#{head} has a marker that does not read (#{d["why"]}) (#{size_words}); #{tail}"
  else
    "#{head} is agent scratch with no .tend-scratch marker (#{size_words}); #{tail}"
  end
end

def reclaim_line(r)
  "tend agent-scratch: ✅ reclaimed #{r["path"]} (#{gib_words(r["bytes"])}, #{r["verdict"]}, owner #{r["owner"]})"
end

def failed_line(r)
  "tend agent-scratch: ❌ could not reclaim #{r["path"]}: #{r["error"]}"
end

@doc """
lines: Every sentence a run earns: each reclaim, each failed reclaim, each piece of debris.

    lines({"reclaimed" => [], "failed" => [], "debris" => []}) #=> []
"""
def lines(run)
  append(
    map(reclaim_line, run["reclaimed"]),
    map(failed_line, run["failed"]),
    map(debris_line, run["debris"])
  )
end

@doc """
beat: The heartbeat verdict plantao reads: failed when a reclaim failed, degraded when debris passes `debris_alert_gib`, else ok, with the detail in one sentence.

    beat({"failed" => 0, "debris_bytes" => 0, "debris_count" => 0, "scratch_bytes" => 0, "scratch_count" => 0, "oldest_age_secs" => 0}, {"debris_alert_gib" => 1})["verdict"] #=> "ok"
"""
def beat(summary, s)
  alert = s["debris_alert_gib"] * gib
  held = "#{summary["scratch_count"]} scratch dir(s), #{gib_words(summary["scratch_bytes"])}, oldest #{hours_words(summary["oldest_age_secs"])}"
  debris = "#{summary["debris_count"]} undeclared dir(s), #{gib_words(summary["debris_bytes"])}"
  if summary["failed"] > 0
    {
      "verdict" => "failed",
      "detail" => "#{summary["failed"]} reclaim(s) failed; #{held}; #{debris}"
    }
  elsif alert > 0 and summary["debris_bytes"] >= alert
    {
      "verdict" => "degraded",
      "detail" => "#{debris} need the operator (tend agent-scratch report); #{held}"
    }
  else
    {"verdict" => "ok", "detail" => "#{held}; #{debris}"}
  end
end

test "an expired marked dir is reclaimed, a fresh one is kept"
  s = settings({})["settings"]
  out = verdicts(
    [
      cand(10, 9, 9 * hour, 3 * hour, "free"),
      cand(10, 9, 9 * hour, 24 * hour, "free")
    ],
    s,
    false
  )
  assert out == %w[remove-expired keep]
end

test "a held, unseen or recently written dir is never reclaimed"
  s = settings({})["settings"]
  assert verdicts([cand(1, 9, 99 * hour, hour, "used")], s, false) ==
    ["in-use-process"]
  assert verdicts([cand(1, 9, 99 * hour, hour, "unseen")], s, false) ==
    ["in-use-unseen"]
  assert verdicts([cand(1, 0, 99 * hour, hour, "free")], s, false) ==
    ["in-use-recent"]
end

test "idle past the limit is reclaimed though its ttl stands"
  s = settings({"max_idle_hours" => 10})["settings"]
  assert verdicts([cand(1, 11, 11 * hour, 999 * hour, "free")], s, false) ==
    ["remove-idle"]
end

test "pressure shortens the ttl, never below the floor"
  s = settings({})["settings"]
  assert verdicts([cand(1, 9, 9 * hour, 24 * hour, "free")], s, false) ==
    ["keep"]
  assert verdicts([cand(1, 9, 7 * hour, 24 * hour, "free")], s, true) ==
    ["remove-expired"]
  assert verdicts([cand(1, 9, 5 * hour, 24 * hour, "free")], s, true) ==
    ["keep"]
end

test "a bad field is refused alone and its siblings stand"
  out = settings({"floor_hours" => -1, "max_idle_hours" => 5, "nope" => 1})
  assert out["settings"]["floor_hours"] == 6
  assert out["settings"]["max_idle_hours"] == 5
  assert size(out["refusals"]) == 2
end

test "a marker needs an owner, a creation time and a ttl"
  assert marker_whys(
    {"owner" => "s", "created_unix" => 1, "ttl_hours" => 1, "purpose" => "x"}
  ) == []
  assert size(
    marker_whys({"owner" => "s", "created_unix" => 1, "ttl_hours" => 0})
  ) == 1
  assert size(
    marker_whys(
      {"owner" => "s", "created_unix" => 1, "ttl_hours" => 1, "x" => 1}
    )
  ) == 1
  assert marker_whys(7) == ["the marker is not a record"]
end
