"""`understudy` entry point."""

from __future__ import annotations

import argparse
import os
import sys

from understudy.sources.claude_code import resolve_session
from understudy.tui.app import UnderstudyApp


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="understudy",
        description="Read-only side-car that maintains a live understanding of a coding-agent session.",
    )
    parser.add_argument(
        "--here",
        action="store_true",
        help="Only show sessions whose cwd matches the current directory.",
    )
    parser.add_argument(
        "--session",
        metavar="UUID_OR_PATH",
        help="Open a session directly, skipping the picker.",
    )
    args = parser.parse_args()

    cwd_filter = os.getcwd() if args.here else None

    session_path = None
    if args.session:
        session_path = resolve_session(args.session)
        if session_path is None:
            print(f"error: no session found for {args.session!r}", file=sys.stderr)
            raise SystemExit(1)

    UnderstudyApp(cwd_filter=cwd_filter, session_path=session_path).run()


if __name__ == "__main__":
    main()
