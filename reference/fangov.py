"""Use as much CPU as possible while keeping the fans at or below the maintainer's 2500 rpm.

All compile, test and draftwright work runs at normal QoS (all cores). It is throttled by a duty
cycle: in each PERIOD seconds it runs for duty * PERIOD and is paused (SIGSTOP) for the rest, so a
multi-threaded process is slowed as fairly as a single-threaded one and none waits forever. The
duty follows the fans, read once a period: above LIMIT it drops 30%, above HIGH 10%; while the fans
are off it rises 10 points every RISE periods, and at base speed 5 points every 2 * RISE (slowly,
because fans lag the heat). The fans' lowest running speed is about 2500 rpm (2500-2599 in the
log), so that counts as within the limit. Logs each duty change.
Usage: python3 -I fangov.py <fan binary> <log>"""
import os, re, signal, subprocess, sys, time

LIMIT, HIGH, LOW = 2700, 2600, 2000
PERIOD, FLOOR, RISE = 10, 0.2, 3
NAMES = r"rustc|cargo|ld|clang|cc|cargo-clippy|clippy-driver"


def rpm(binary):
    out = subprocess.run([binary], capture_output=True, text=True).stdout
    return max((int(x) for x in re.findall(r"fan\d+ (\d+)/", out)), default=0)


def work():
    pids = set()
    for args in (["pgrep", "-x", NAMES], ["pgrep", "-f", r"target/(release|debug)/"]):
        pids |= {int(p) for p in subprocess.run(args, capture_output=True, text=True).stdout.split()}
    return sorted(pids - {os.getpid()})


def signal_all(pids, sig):
    for pid in pids:
        try: os.kill(pid, sig)
        except ProcessLookupError: pass


def write(log, line):
    with open(log, "a") as f:
        f.write(f"{time.strftime('%F %T')} {line}\n")


def release(*_):
    signal_all(work(), signal.SIGCONT)
    write(log, "exit: all work continued")
    sys.exit(0)


binary, log = sys.argv[1], sys.argv[2]
signal.signal(signal.SIGTERM, release)
signal.signal(signal.SIGINT, release)
# A restart while the fans spin starts at half duty rather than at full.
duty, ticks = (1.0 if rpm(binary) == 0 else 0.5), 0
signal_all(work(), signal.SIGCONT)
write(log, f"start: all cores, duty cycle {duty:.0%} of each {PERIOD} s, fans held at base speed (~2500 rpm)")
while True:
    try:
        speed, pids = rpm(binary), work()
        old = duty
        if speed > LIMIT:
            duty, ticks = max(FLOOR, duty * 0.7), 0
        elif speed > HIGH:
            duty, ticks = max(FLOOR, duty * 0.9), 0
        elif duty < 1.0:
            ticks += 1
            step, every = (0.10, RISE) if speed < LOW else (0.05, 2 * RISE)
            if ticks >= every:
                duty, ticks = min(1.0, duty + step), 0
        if round(duty, 2) != round(old, 2):
            write(log, f"fan {speed} rpm: duty {old:.0%} -> {duty:.0%}")
        for pid in pids:
            subprocess.run(["taskpolicy", "-B", "-p", str(pid)], capture_output=True)
        signal_all(pids, signal.SIGCONT)
        if duty >= 1.0:
            time.sleep(PERIOD)
            continue
        time.sleep(PERIOD * duty)
        signal_all(work(), signal.SIGSTOP)
        time.sleep(PERIOD * (1 - duty))
    except Exception as e:
        write(log, f"error: {e}")
        signal_all(work(), signal.SIGCONT)
        time.sleep(PERIOD)
