"""The retry schedule of the pinned-download actions (stdlib only).

Each setup action downloads its tool with curl. Without `--retry-delay` curl
waits 1 s before the first retry and doubles the wait for each next one, so the
retry count sets how long a server outage the download outlasts.
"""

import re
import unittest
from pathlib import Path

ACTIONS = Path(__file__).resolve().parents[2] / ".github" / "actions"
DOWNLOAD_ACTIONS = ("setup-mold", "setup-sccache", "setup-zig")
# The total backoff the retries must span: long enough for a short outage of
# the release CDN, short enough that a dead download fails within minutes.
BACKOFF_RANGE_SECONDS = (60, 180)


def env_value(action: str, name: str) -> str | None:
    match = re.search(rf'^ {{8}}{re.escape(name)}: "([^"]*)"$', action, re.M)
    return None if match is None else match.group(1)


class DownloadRetryScheduleTests(unittest.TestCase):
    def test_every_download_backs_off_over_one_to_three_minutes(self):
        for name in DOWNLOAD_ACTIONS:
            with self.subTest(action=name):
                action = (ACTIONS / name / "action.yml").read_text()
                # A fixed delay turns curl's exponential backoff off.
                self.assertNotIn("--retry-delay", action.replace("Without --retry-delay", ""))
                self.assertIn('--retry "$DOWNLOAD_RETRIES" --retry-all-errors', action)
                retries = int(env_value(action, "DOWNLOAD_RETRIES"))
                backoff = sum(2**attempt for attempt in range(retries))
                self.assertGreaterEqual(backoff, BACKOFF_RANGE_SECONDS[0])
                self.assertLessEqual(backoff, BACKOFF_RANGE_SECONDS[1])
                # The overall retry budget, where one is set, leaves room for
                # the whole backoff.
                budget = env_value(action, "DOWNLOAD_RETRY_MAX_TIME_SECONDS")
                if budget is not None:
                    self.assertGreater(int(budget), backoff)


if __name__ == "__main__":
    unittest.main()
