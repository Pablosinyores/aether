#!/usr/bin/env python3
"""Scrape-staleness canary for Aether.

The Prometheus side of the stack alerts on metric values; it cannot tell us
that a service has *stopped* emitting altogether (a dead process looks the
same as a process reporting zero). This probe GETs the metrics and stats
endpoints on a fixed cadence, tracks the last success per target, and
pushes a `ScrapeStale` alert to Alertmanager when a target has been
unresponsive for longer than the threshold. Recovery sends a resolved
alert so Alertmanager can clear the Slack notification.

Config is via environment variables:

    CANARY_TARGETS        Comma-separated `host:port/path` entries.
                          Default: the three compose-internal endpoints.
    ALERTMANAGER_URL      Base URL of Alertmanager.
                          Default: http://alertmanager:9093
    PROBE_INTERVAL_SEC    Seconds between probe rounds. Default 60.
    STALE_AFTER_SEC       Alert after this many seconds without a 2xx.
                          Default 180 (three probe rounds).
    PROBE_TIMEOUT_SEC     Per-request timeout. Default 10.

The script only uses the standard library so it ships in a minimal image.
"""

from __future__ import annotations

import json
import os
import signal
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Dict, List


DEFAULT_TARGETS = (
    "aether-rust:9092/metrics,"
    "aether-go:9090/metrics,"
    "aether-go:8080/api/stats"
)


@dataclass
class Target:
    endpoint: str
    last_success: float | None = None
    firing: bool = False


@dataclass
class Config:
    targets: List[Target]
    alertmanager_url: str
    probe_interval_sec: int
    stale_after_sec: int
    probe_timeout_sec: int


def load_config() -> Config:
    raw = os.environ.get("CANARY_TARGETS", DEFAULT_TARGETS)
    endpoints = [t.strip() for t in raw.split(",") if t.strip()]
    return Config(
        targets=[Target(endpoint=e) for e in endpoints],
        alertmanager_url=os.environ.get(
            "ALERTMANAGER_URL", "http://alertmanager:9093"
        ).rstrip("/"),
        probe_interval_sec=int(os.environ.get("PROBE_INTERVAL_SEC", "60")),
        stale_after_sec=int(os.environ.get("STALE_AFTER_SEC", "180")),
        probe_timeout_sec=int(os.environ.get("PROBE_TIMEOUT_SEC", "10")),
    )


def probe(target: Target, timeout: int) -> bool:
    url = target.endpoint
    if not url.startswith("http://") and not url.startswith("https://"):
        url = f"http://{url}"
    req = urllib.request.Request(url, headers={"User-Agent": "aether-canary/0.1"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return 200 <= resp.status < 300
    except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError):
        return False
    except OSError:
        return False


def iso_now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def post_alert(
    am_url: str,
    target: Target,
    age_sec: float,
    resolved: bool,
    timeout: int,
) -> None:
    now = iso_now()
    alert: Dict[str, object] = {
        "labels": {
            "alertname": "ScrapeStale",
            "service": target.endpoint,
            "severity": "critical",
            "source": "canary",
        },
        "annotations": {
            "summary": f"{target.endpoint} has not responded to canary probes",
            "description": (
                f"Canary could not reach {target.endpoint} for {int(age_sec)}s. "
                f"This typically means the process is down or unreachable."
            ),
        },
        "startsAt": now,
    }
    if resolved:
        alert["endsAt"] = now
    body = json.dumps([alert]).encode("utf-8")
    req = urllib.request.Request(
        f"{am_url}/api/v2/alerts",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            if resp.status >= 300:
                print(
                    f"[canary] alertmanager returned {resp.status} for {target.endpoint}",
                    file=sys.stderr,
                )
    except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError, OSError) as e:
        print(
            f"[canary] failed to post alert to alertmanager: {e}",
            file=sys.stderr,
        )


_RUNNING = True


def _handle_signal(signum, _frame):
    global _RUNNING
    print(f"[canary] received signal {signum}, exiting", file=sys.stderr)
    _RUNNING = False


def run(cfg: Config) -> None:
    signal.signal(signal.SIGINT, _handle_signal)
    signal.signal(signal.SIGTERM, _handle_signal)

    print(
        f"[canary] probing {len(cfg.targets)} targets every {cfg.probe_interval_sec}s, "
        f"stale after {cfg.stale_after_sec}s",
        file=sys.stderr,
    )

    while _RUNNING:
        cycle_start = time.monotonic()
        for target in cfg.targets:
            ok = probe(target, cfg.probe_timeout_sec)
            now = time.monotonic()
            if ok:
                target.last_success = now
                if target.firing:
                    post_alert(
                        cfg.alertmanager_url, target, 0, resolved=True,
                        timeout=cfg.probe_timeout_sec,
                    )
                    target.firing = False
                    print(
                        f"[canary] {target.endpoint} recovered",
                        file=sys.stderr,
                    )
                continue

            age = (now - target.last_success) if target.last_success else cfg.stale_after_sec + 1
            if age >= cfg.stale_after_sec and not target.firing:
                post_alert(
                    cfg.alertmanager_url, target, age, resolved=False,
                    timeout=cfg.probe_timeout_sec,
                )
                target.firing = True
                print(
                    f"[canary] {target.endpoint} stale for {int(age)}s, alert fired",
                    file=sys.stderr,
                )

        elapsed = time.monotonic() - cycle_start
        sleep_for = max(0.0, cfg.probe_interval_sec - elapsed)
        end = time.monotonic() + sleep_for
        while _RUNNING and time.monotonic() < end:
            time.sleep(min(1.0, end - time.monotonic()))


def self_test() -> int:
    cfg = load_config()
    print(f"targets: {[t.endpoint for t in cfg.targets]}")
    print(f"alertmanager: {cfg.alertmanager_url}")
    print(f"interval: {cfg.probe_interval_sec}s stale_after: {cfg.stale_after_sec}s")
    return 0


def main(argv: List[str]) -> int:
    if len(argv) > 1 and argv[1] == "--self-test":
        return self_test()
    cfg = load_config()
    if not cfg.targets:
        print("[canary] no targets configured", file=sys.stderr)
        return 2
    run(cfg)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
