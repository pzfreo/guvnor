# fangov

Keeps a Mac's fans at or below a target speed (say 2500 rpm) while heavy background work —
builds, tests, renders — uses as much CPU as that allows. It throttles only the work processes,
never the whole machine: each 10 s period the work runs for `duty × period` and is paused with
SIGSTOP for the rest, and the duty follows the fans.

## How it differs

- **App Tamer** throttles per app to a CPU percentage you choose. fangov chooses the amount
  itself, from the fans, and finds short-lived build processes as they appear.
- **TG Pro / Macs Fan Control** set fan speeds or curves. They make the fans louder or quieter for
  the same heat; fangov makes less heat.
- **Low Power Mode** slows everything, including what you are using. fangov leaves the rest of the
  machine at full speed, and the work runs at normal priority on all cores (performance and
  efficiency) whenever it runs.

## Install

```sh
cargo install --path .        # puts fangov in ~/.cargo/bin
fangov config > ~/.config/fangov/config.toml   # optional: the defaults, commented
fangov run                    # try it in the foreground first; Ctrl-C continues all work
fangov install                # then run it as a per-user LaunchAgent (KeepAlive)
```

The LaunchAgent logs to `~/Library/Logs/fangov.log`. Needs no root: it reads the SMC through
IOKit and only signals your own processes.

## Commands

| command | |
|---|---|
| `fangov run` | run in the foreground, logging each duty change with the rpm |
| `fangov status` | the governor's duty, the fans, the power source, the matched processes |
| `fangov pause` | throttling off and every work process continued, until `resume` (survives restarts) |
| `fangov resume` | throttling on |
| `fangov install` / `uninstall` | add / remove the LaunchAgent |
| `fangov config` | print the default config |
| `fangov fans` | print the fans, e.g. `fan0 2312/7826 fan1 2495/7826` |

## Configuration

`~/.config/fangov/config.toml` (or `$FANGOV_CONFIG`). Every setting is optional; unknown keys are
an error. `fangov config` prints them all with comments. In short:

- `target_rpm` (2500), `period` (10 s).
- `[rules]`: above `target + limit_above` (2700) duty × 0.7; above `target + high_above` (2600)
  duty × 0.9; fans off (below 2000) +10 points every 3 periods; at base speed +5 points every 6;
  `floor` 0.2, `ceiling` 1.0; `cut_hold` and `relearn_every` (below).
- `[work]`: `names` (whole process name, like `pgrep -x`; default the Rust/C toolchain), `cmdlines`
  (command-line search, like `pgrep -f`; default `target/(release|debug)/`), and `tree_names` /
  `tree_cmdlines` (that process and all its descendants).
- `[battery]` (off unless present): `max_duty`, `target_rpm`, `below_charge`.

The governor reads the config at start; restart it after a change (`launchctl kickstart -k
gui/$(id -u)/local.fangov` for the LaunchAgent).

## The control loop

These rules are the prototype's, which ran for days on a Mac17,7 (M5 Max). These fans are either
off or at least about 2500 rpm (base speed reads 2500–2599), so a 2500 target means "base speed is
fine, never higher". The fans also lag heat by tens of seconds.

Simulated against a fan with that lag, the prototype's rules settle into a limit cycle. The duty
climbs at base speed until the fans pass 2600; then, because the fans keep rising on the heat from
before the cut, it cuts three or four times in a row and climbs again. That gives a cycle of about
6 minutes. Two additions stop it, and `cut_hold = 0`, `relearn_every = 0` turns both off:

- **cut_hold** (4 periods): after a cut, wait before cutting again. Below LIMIT, also only cut
  again if the fans are not already falling.
- **relearn_every** (30 periods): a cut remembers the duty that overshot, and rises stop one
  base step below it. After 30 periods there at base speed it tries one step higher. A try that
  overshoots doubles the wait, up to 8× (40 minutes). With the fans off it tries faster.

In the simulation at full load, the new rules hold the fans at about 2580 rpm with the duty steady
near 60–65%. The only excursions left are the occasional tries, which peak around 2620 rpm.

At duty 100% the governor stops nothing and does not even look for work; it only polls the fans.
Work it finds is moved out of the background band (`taskpolicy -B`), so nothing is left on the
efficiency cores only.

## Crash safety

If the governor died during a pause, the work would stay stopped for good. Three layers prevent
this:

1. **On start** it continues every matching process, and everything the last governor recorded.
2. **It records every pause** in `~/.local/state/fangov/stopped.json` before sending SIGSTOP. A
   separate watchdog process (in its own session, so Ctrl-C and terminal signals don't reach it)
   continues them within a quarter second if the governor exits or is killed. If both are killed,
   the LaunchAgent's restart continues them, and so does `fangov status` or `fangov pause`.
3. **Every pause is bounded**: no longer than `period × (1 − floor)` (8 s by default). If the
   governor hangs, the watchdog continues the work at that deadline (plus half a second).

It never pauses itself, its watchdog, its ancestors (your shell and terminal), any `fangov`,
another user's process, or a process already stopped by someone else (Ctrl-Z).

## Risks

- **Throttled work takes longer in wall-clock time.** Tests with wall-time timeouts can fail, and
  timing benchmarks are meaningless while throttled. Measure work in instructions retired
  (`/usr/bin/time -l`), not wall time, or `fangov pause` first.
- **Matching is by name and command line.** The default `target/(release|debug)/` pattern matches
  any of your processes with that in its command line, including a shell running such a command.
  Pausing a waiting shell is harmless. Check `fangov status` after changing the matchers.
- **A process paused mid-network-exchange** can time out at the other end, as with any SIGSTOP.
- If both the governor and its watchdog are killed while no LaunchAgent is installed (a
  foreground `fangov run`), work stays paused until you run `fangov status`, `fangov pause` or
  `fangov run`, or `kill -CONT` it.

## Uninstall

```sh
fangov uninstall          # stops the agent (it continues all work) and removes the plist
cargo uninstall fangov
rm -rf ~/.config/fangov ~/.local/state/fangov ~/Library/Logs/fangov.log
```

## Tests

`cargo test`: the controller against a simulated lagging fan (it converges at a 2500 target, beats
the prototype's rules, and follows load changes), the config, the process matcher, and the crash
safety of the real binary. Those tests kill the governor with SIGKILL mid-pause, kill it together
with its watchdog, hang it with SIGSTOP, stop it with SIGTERM, and use `pause`/`resume`, checking
each time that no process stays stopped. They throttle only their own `sleep` processes.

`reference/` holds the prototype (`fan.c`, `fangov.py`) this was ported from.
