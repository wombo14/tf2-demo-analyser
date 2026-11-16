"""Prints the newest entry of CHANGES.txt's history as Markdown, for a release's notes.

    python packaging/release_notes.py CHANGES.txt

The history is a list of dated entries under "HISTORY (newest first)". Each entry
is a date line, then "  - " items whose text continues on lines indented by four.
"""
import re
import sys

DATE = re.compile(r"^\d{1,2} [A-Z][a-z]+ \d{4}\b")


def newest_entry(text):
    lines = text.splitlines()
    try:
        start = next(i for i, line in enumerate(lines) if line.startswith("HISTORY"))
    except StopIteration:
        return None, []
    date, items = None, []
    for line in lines[start + 1:]:
        if DATE.match(line.strip()) and not line.startswith(" "):
            if date is not None:
                break
            date = line.strip().rstrip(".")
        elif date is None:
            continue
        elif line.startswith("  - "):
            items.append(line[4:].strip())
        elif line.startswith("    ") and items:
            items[-1] += " " + line.strip()
    return date, items


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "CHANGES.txt"
    with open(path, encoding="utf-8") as file:
        date, items = newest_entry(file.read())
    if not items:
        return
    print(f"## What's new ({date})")
    print()
    for item in items:
        print(f"- {item}")


if __name__ == "__main__":
    main()
