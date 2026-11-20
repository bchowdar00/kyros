#!/usr/bin/env python3
"""Build replay files for finished NBA games.

For each date on the command line this fetches the ESPN scoreboard, the ESPN
play-by-play summary of every completed game, and Kalshi's candlesticks for the
home team's win market, then writes one compact JSON file per game under
``public/replays`` plus an index at ``src/features/replay/replay-index.json``.

Usage (run from ``frontend``):
    python3 -I scripts/build_replays.py 20261117 20261118 20261119

Python 3 standard library only. Network access is restricted to the two hosts
below; nothing else is contacted and no API keys are used.
"""

from __future__ import annotations

import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path

ESPN_HOST = "site.api.espn.com"
KALSHI_HOST = "api.elections.kalshi.com"
ALLOWED_HOSTS = {ESPN_HOST, KALSHI_HOST}

# A play's gap to the previous play is clamped to this, so dead time between
# plays (timeouts, reviews, breaks) never stretches the shortened timeline.
MAX_GAP_MS = 8000
# A game needs at least this many plays and this many usable prices to be kept.
MIN_PLAYS = 100
MIN_PRICES = 30

# ESPN abbreviation -> Kalshi code, tried only after the as-is code fails.
KALSHI_ALIAS = {
    "GS": "GSW",
    "NY": "NYK",
    "SA": "SAS",
    "NO": "NOP",
    "UTAH": "UTA",
    "WSH": "WAS",
}
MONTHS = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"]

HERE = Path(__file__).resolve().parent
FRONTEND = HERE.parent
REPLAYS_DIR = FRONTEND / "public" / "replays"
INDEX_PATH = FRONTEND / "src" / "features" / "replay" / "replay-index.json"


class SkipGame(Exception):
    """A game that cannot be built; carries a one-line reason."""


def fetch_json(url: str) -> dict:
    """GET ``url`` as JSON. Only the two allowed hosts; sleeps 0.3s, one retry."""
    host = urllib.parse.urlparse(url).hostname
    if host not in ALLOWED_HOSTS:
        raise SkipGame(f"refused host {host}")

    last_error: Exception | None = None
    for attempt in range(2):
        time.sleep(0.3)
        try:
            request = urllib.request.Request(url, headers={"User-Agent": "replay-builder/1.0"})
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.loads(response.read().decode("utf-8"))
        except (urllib.error.URLError, ValueError, TimeoutError) as error:
            last_error = error
    raise SkipGame(f"request failed: {last_error}")


def wallclock_ms(play: dict) -> int:
    """A play's wallclock as epoch milliseconds."""
    stamp = datetime.strptime(play["wallclock"], "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
    return int(stamp.timestamp() * 1000)


def kalshi_date(first_wall_ms: int) -> str:
    """The Kalshi/New-York date of a game, e.g. ``26NOV19`` from the first play."""
    when = datetime.fromtimestamp(first_wall_ms / 1000, tz=timezone.utc) - timedelta(hours=5)
    return f"{when.year % 100:02d}{MONTHS[when.month - 1]}{when.day:02d}"


def ny_date(first_wall_ms: int) -> str:
    """The New-York calendar date of a game as ``YYYY-MM-DD``."""
    when = datetime.fromtimestamp(first_wall_ms / 1000, tz=timezone.utc) - timedelta(hours=5)
    return when.strftime("%Y-%m-%d")


def code_candidates(code: str) -> list[str]:
    """The code tried as-is first, then through the alias table."""
    upper = code.upper()
    out = [upper]
    alias = KALSHI_ALIAS.get(upper)
    if alias is not None and alias != upper:
        out.append(alias)
    return out


def build_plays(raw_plays: list[dict]) -> tuple[list[dict], list[int]]:
    """Shortened-timeline plays plus each play's real epoch-ms time.

    Returns the output plays and a parallel list of real wallclock times.
    """
    first_ms = wallclock_ms(raw_plays[0])
    plays: list[dict] = []
    real_ms: list[int] = []
    running = 0
    prev_ms = first_ms
    prev_home = 0
    prev_away = 0
    for index, raw in enumerate(raw_plays):
        this_ms = wallclock_ms(raw)
        gap = this_ms - prev_ms
        if gap < 0:
            gap = 0
        running += min(gap, MAX_GAP_MS)
        prev_ms = this_ms

        home = int(raw["homeScore"])
        away = int(raw["awayScore"])
        if index == 0:
            points = home + away
            delta_home = home
            delta_away = away
        else:
            delta_home = home - prev_home
            delta_away = away - prev_away
            points = delta_home + delta_away
        if delta_home > 0:
            team = "home"
        elif delta_away > 0:
            team = "away"
        else:
            team = None
        prev_home = home
        prev_away = away

        plays.append(
            {
                "atMs": running,
                "period": int(raw["period"]["number"]),
                "clock": str(raw["clock"]["displayValue"]),
                "home": home,
                "away": away,
                "team": team,
                "points": points,
            }
        )
        real_ms.append(this_ms)
    return plays, real_ms


def usable_candle(candle: dict) -> tuple[float, float] | None:
    """The (bid, ask) of a candle when both parse and ``0 < bid < ask < 1``."""
    try:
        bid = float(candle["yes_bid"]["close_dollars"])
        ask = float(candle["yes_ask"]["close_dollars"])
    except (KeyError, TypeError, ValueError):
        return None
    if 0 < bid < ask < 1:
        return (bid, ask)
    return None


def interpolate_at_ms(real_time: int, plays: list[dict], real_ms: list[int]) -> int:
    """Place a real epoch-ms time onto the shortened timeline between two plays."""
    for i in range(len(plays) - 1):
        left = real_ms[i]
        right = real_ms[i + 1]
        if left <= real_time <= right:
            span = right - left
            if span <= 0:
                return plays[i]["atMs"]
            fraction = (real_time - left) / span
            at = plays[i]["atMs"] + fraction * (plays[i + 1]["atMs"] - plays[i]["atMs"])
            return round(at)
    # At or past the last play: clamp to the end.
    return plays[-1]["atMs"]


def build_prices(
    candles: list[dict], plays: list[dict], real_ms: list[int]
) -> list[dict]:
    """Prices on the shortened timeline from the usable candles."""
    first_real = real_ms[0]
    last_real = real_ms[-1]
    prices: list[dict] = []

    # The last usable candle at or before the first play becomes atMs 0.
    opener: tuple[float, float] | None = None
    for candle in candles:
        parsed = usable_candle(candle)
        if parsed is None:
            continue
        real_time = int(candle["end_period_ts"]) * 1000
        if real_time <= first_real:
            opener = parsed
    if opener is not None:
        prices.append({"atMs": 0, "bid": opener[0], "ask": opener[1]})

    # Each usable candle between the first and last play, interpolated.
    for candle in candles:
        parsed = usable_candle(candle)
        if parsed is None:
            continue
        real_time = int(candle["end_period_ts"]) * 1000
        if real_time <= first_real or real_time > last_real:
            continue
        at_ms = interpolate_at_ms(real_time, plays, real_ms)
        prices.append({"atMs": at_ms, "bid": parsed[0], "ask": parsed[1]})

    prices.sort(key=lambda price: price["atMs"])
    return prices


def kalshi_prices(
    away_code: str, home_code: str, plays: list[dict], real_ms: list[int]
) -> list[dict]:
    """Fetch and place Kalshi candlesticks for the home team's win market."""
    date = kalshi_date(real_ms[0])
    home_market: str | None = None
    for away_try in code_candidates(away_code):
        for home_try in code_candidates(home_code):
            event_ticker = f"KXNBAGAME-{date}{away_try}{home_try}"
            url = f"https://{KALSHI_HOST}/trade-api/v2/markets?event_ticker={event_ticker}"
            markets = fetch_json(url).get("markets") or []
            if not markets:
                continue
            wanted = f"{event_ticker}-{home_try}"
            if any(market.get("ticker") == wanted for market in markets):
                home_market = wanted
                break
        if home_market is not None:
            break
    if home_market is None:
        raise SkipGame("no Kalshi market found")

    start_ts = real_ms[0] // 1000 - 600
    end_ts = real_ms[-1] // 1000 + 60
    url = (
        f"https://{KALSHI_HOST}/trade-api/v2/series/KXNBAGAME/markets/{home_market}"
        f"/candlesticks?start_ts={start_ts}&end_ts={end_ts}&period_interval=1"
    )
    candles = fetch_json(url).get("candlesticks") or []
    return build_prices(candles, plays, real_ms)


def build_game(event: dict) -> dict:
    """Build one game's replay payload, or raise SkipGame with a reason."""
    event_id = str(event["id"])
    competition = event["competitions"][0]
    sides: dict[str, dict] = {}
    for competitor in competition["competitors"]:
        sides[competitor["homeAway"]] = {
            "code": competitor["team"]["abbreviation"],
            "name": competitor["team"]["displayName"],
        }
    if "home" not in sides or "away" not in sides:
        raise SkipGame("missing home/away competitor")

    summary = fetch_json(
        f"https://{ESPN_HOST}/apis/site/v2/sports/basketball/nba/summary?event={event_id}"
    )
    raw_plays = summary.get("plays") or []
    if len(raw_plays) < MIN_PLAYS:
        raise SkipGame(f"only {len(raw_plays)} plays")
    if any("wallclock" not in play for play in raw_plays):
        raise SkipGame("a play is missing wallclock")

    plays, real_ms = build_plays(raw_plays)
    prices = kalshi_prices(sides["away"]["code"], sides["home"]["code"], plays, real_ms)
    if len(prices) < MIN_PRICES:
        raise SkipGame(f"only {len(prices)} prices")

    last_play = plays[-1]
    return {
        "id": event_id,
        "date": ny_date(real_ms[0]),
        "home": sides["home"],
        "away": sides["away"],
        "durationMs": last_play["atMs"],
        "final": {"home": last_play["home"], "away": last_play["away"]},
        "plays": plays,
        "prices": prices,
    }


def completed_events(date: str) -> list[dict]:
    """The completed events on a scoreboard date."""
    url = (
        f"https://{ESPN_HOST}/apis/site/v2/sports/basketball/nba/scoreboard?dates={date}"
    )
    scoreboard = fetch_json(url)
    out = []
    for event in scoreboard.get("events") or []:
        status = event.get("status", {}).get("type", {})
        if status.get("completed") is True:
            out.append(event)
    return out


def main(dates: list[str]) -> int:
    REPLAYS_DIR.mkdir(parents=True, exist_ok=True)
    INDEX_PATH.parent.mkdir(parents=True, exist_ok=True)

    built: list[dict] = []
    rows: list[tuple] = []
    for date in dates:
        try:
            events = completed_events(date)
        except SkipGame as error:
            print(f"skip scoreboard {date}: {error}")
            continue
        for event in events:
            event_id = str(event["id"])
            try:
                game = build_game(event)
            except SkipGame as error:
                print(f"skip {event_id}: {error}")
                continue
            except (KeyError, IndexError, TypeError) as error:
                print(f"skip {event_id}: bad shape: {error}")
                continue
            payload = json.dumps(game, separators=(",", ":"))
            path = REPLAYS_DIR / f"{event_id}.json"
            path.write_text(payload, encoding="utf-8")
            built.append(game)
            rows.append(
                (
                    event_id,
                    f"{game['away']['code']} @ {game['home']['code']}",
                    len(game["plays"]),
                    len(game["prices"]),
                    game["durationMs"],
                    len(payload),
                )
            )

    def margin(game: dict) -> int:
        return abs(game["final"]["home"] - game["final"]["away"])

    built.sort(key=lambda game: (margin(game), game["id"]))
    index = [
        {
            "id": game["id"],
            "date": game["date"],
            "home": game["home"],
            "away": game["away"],
            "durationMs": game["durationMs"],
            "final": game["final"],
        }
        for game in built
    ]
    INDEX_PATH.write_text(json.dumps(index, separators=(",", ":")), encoding="utf-8")

    print("\nid           teams        plays prices durationMs    bytes")
    for row in rows:
        print(f"{row[0]:<12} {row[1]:<12} {row[2]:>5} {row[3]:>6} {row[4]:>10} {row[5]:>8}")
    print(f"\nbuilt {len(built)} games, wrote index with {len(index)} entries")
    return 0 if built else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
